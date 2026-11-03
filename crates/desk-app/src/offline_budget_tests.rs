#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::test_provider::MockSpeech;
use desk_core::{BillingUnit, Chapter};
use std::{path::PathBuf, sync::Arc};
use tokio::time::timeout;

struct Fixture {
    root: PathBuf,
    manager: Arc<OfflineManager>,
    provider: Arc<MockSpeech>,
}
impl Fixture {
    fn new(chapters: &[&str]) -> Self {
        let root = std::env::temp_dir().join(format!("readio-budget-{}", Uuid::new_v4()));
        let store = Arc::new(std::sync::Mutex::new(Store::open(&root).unwrap()));
        {
            let store = store.lock().unwrap();
            let mut prefs = store.preferences().unwrap();
            prefs.voice = Some("openrouter".into());
            prefs.open_router_model = Some("test/model".into());
            prefs
                .open_router_billing_units
                .insert("test/model".into(), BillingUnit::Character);
            store.save_preferences(&prefs).unwrap();
            let mut book = Book {
                id: "book".into(),
                title: "Книга".into(),
                chapters: chapters
                    .iter()
                    .enumerate()
                    .map(|(i, text)| Chapter {
                        title: format!("{i}"),
                        text: (*text).into(),
                    })
                    .collect(),
                ..Book::default()
            };
            book.refresh();
            store.import_parsed(&[book]).unwrap();
        }
        let mut manager = OfflineManager::new(store, root.clone()).unwrap();
        let provider = MockSpeech::new();
        Arc::get_mut(&mut manager).unwrap().test_provider = Some(Arc::clone(&provider));
        Self {
            root,
            manager,
            provider,
        }
    }
    async fn quote(&self) -> QuoteResult {
        self.manager.quote("book".into()).await.unwrap()
    }
    async fn first_block(&self, quote: &QuoteResult) -> (AudioBlock, AudioProfile) {
        let quotes = self.manager.quotes.lock().await;
        let quote = quotes.get(&quote.quote_id).unwrap();
        let result = (
            quote.plan.chapters[0].blocks[0].clone(),
            quote.manifest.profile.clone(),
        );
        drop(quotes);
        result
    }
    async fn start(
        &self,
        quote: &QuoteResult,
        blocks: Option<usize>,
        usd: Option<&str>,
        allow_unknown: bool,
    ) -> BlockResult<OfflineStatus> {
        self.manager
            .start(
                "book".into(),
                quote.quote_id.clone(),
                false,
                OfflineBudget {
                    max_new_blocks: blocks,
                    max_cost_usd: usd.map(str::to_owned),
                    allow_unknown_cost: allow_unknown,
                },
            )
            .await
    }
    async fn wait_stopped(&self) -> OfflineStatus {
        timeout(Duration::from_secs(5), async {
            loop {
                let running = self
                    .manager
                    .jobs
                    .lock()
                    .await
                    .get("book")
                    .is_some_and(|job| job.state == OfflineJobState::Running);
                if !running {
                    return self.manager.status("book").await.unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn cached_after_quote_does_not_consume_submission_budget() {
    let f = Fixture::new(&["Первое предложение."]);
    let quote = f.quote().await;
    let (block, _) = f.first_block(&quote).await;
    let gate = f.manager.gate.lock().await;
    f.start(&quote, Some(1), None, false).await.unwrap();
    timeout(Duration::from_secs(2), async {
        while !f.manager.inflight.lock().await.contains_key(&block.id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.manager
        .cache
        .put(&block.id, "audio/mpeg", b"cached")
        .unwrap();
    drop(gate);
    let status = f.wait_stopped().await;
    assert_eq!(status.state, OfflineJobState::Complete);
    assert_eq!(status.submitted_requests, 0);
    assert_eq!(f.provider.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn new_block_limit_stops_before_second_submission() {
    let f = Fixture::new(&["Первая глава.", "Вторая глава."]);
    let quote = f.quote().await;
    assert_eq!(quote.total_blocks, 2);
    f.start(&quote, Some(1), None, false).await.unwrap();
    let status = f.wait_stopped().await;
    assert_eq!(status.state, OfflineJobState::Paused);
    assert_eq!(status.cached_blocks, 1);
    assert_eq!(status.submitted_requests, 1);
    assert_eq!(f.provider.calls().len(), 1);
}

#[tokio::test]
async fn shared_interactive_and_offline_work_dispatch_once() {
    let f = Fixture::new(&["Общий аудиоблок."]);
    f.provider.hold();
    f.provider.push_usage(Some("0.125"));
    let quote = f.quote().await;
    let (block, profile) = f.first_block(&quote).await;
    f.start(&quote, Some(1), Some("10"), false).await.unwrap();
    timeout(Duration::from_secs(2), f.provider.wait_calls(1))
        .await
        .unwrap();
    let session = f.manager.queue.open_playback_session().unwrap();
    assert!(
        f.manager
            .queue
            .update_playback(&session, 1, HashSet::from([block.id.clone()]))
    );
    let manager = Arc::clone(&f.manager);
    let task = tokio::spawn(async move {
        manager
            .request_block(block, profile, None, None, None)
            .await
    });
    timeout(Duration::from_secs(2), async {
        loop {
            let notified = f.manager.coalesced_signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if f.manager.coalesced_waiters.load(Ordering::SeqCst) > 0 {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
    f.provider.release();
    assert!(
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_some()
    );
    let status = f.wait_stopped().await;
    assert_eq!(status.state, OfflineJobState::Complete);
    assert_eq!(status.submitted_requests, 1);
    assert_eq!(status.actual_cost_usd.as_deref(), Some("0.125"));
    assert_eq!(f.provider.calls().len(), 1);
}

#[tokio::test]
async fn changed_rate_rejects_quote_without_dispatch() {
    let f = Fixture::new(&["Тариф проверяется."]);
    f.provider.set_price("0.0000001", "0", false);
    let quote = f.quote().await;
    f.provider.set_price("0.0000002", "0", false);
    let Err(error) = f.start(&quote, Some(1), Some("1"), false).await else {
        panic!("changed rate accepted");
    };
    assert_eq!(error.code, BlockErrorCode::StaleQuote);
    assert_eq!(f.provider.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn unknown_cost_survives_restart_and_reconciles_without_another_tts_request() {
    let f = Fixture::new(&["Первая глава.", "Вторая глава."]);
    f.provider.push_usage(None);
    let quote = f.quote().await;
    f.start(&quote, Some(2), Some("10"), false).await.unwrap();
    let paused = f.wait_stopped().await;
    assert_eq!(paused.state, OfflineJobState::Paused);
    assert_eq!(paused.unknown_cost_requests, 1);
    assert_eq!(f.provider.calls().len(), 1);
    f.manager.begin_shutdown();
    timeout(Duration::from_secs(2), f.manager.shutdown())
        .await
        .unwrap();
    let mut cold = OfflineManager::new(Arc::clone(&f.manager.store), f.root.clone()).unwrap();
    Arc::get_mut(&mut cold).unwrap().test_provider = Some(Arc::clone(&f.provider));
    let before = cold.status("book").await.unwrap();
    assert_eq!(before.unknown_cost_requests, 1);
    assert_eq!(before.submitted_requests, 1);
    let manifest = cold.manifests().await.unwrap().into_iter().next().unwrap();
    let attempts = cold
        .journal
        .lock()
        .unwrap()
        .attempts_for_blocks(&manifest.block_ids)
        .unwrap();
    let attempt = attempts
        .into_iter()
        .find(|attempt| attempt.provider_response_id.is_some())
        .unwrap();
    let response = attempt.provider_response_id.clone().unwrap();
    f.provider
        .set_usage(&response, Some("0.100000000000000001"));
    let after = cold.reconcile_book_cost("book").await.unwrap();
    assert_eq!(after.unknown_cost_requests, 0);
    assert_eq!(
        after.actual_cost_usd.as_deref(),
        Some("0.100000000000000001")
    );
    assert_eq!(f.provider.calls().len(), 1);
}

#[tokio::test]
async fn repeat_attempts_sum_exactly_and_remain_visible_after_restart() {
    let f = Fixture::new(&["Проверка повторной попытки."]);
    f.provider.push_usage(Some("0.100000000000000001"));
    let quote = f.quote().await;
    let (block, _) = f.first_block(&quote).await;
    f.start(&quote, Some(1), Some("10"), false).await.unwrap();
    assert_eq!(f.wait_stopped().await.state, OfflineJobState::Complete);
    std::fs::remove_file(
        f.root
            .join("audio-cache/blocks")
            .join(format!("{}.audio", block.id)),
    )
    .unwrap();
    f.provider.push_usage(Some("0.200000000000000002"));
    let retry = f.quote().await;
    assert_eq!(retry.retry_required_blocks, 1);
    f.manager
        .start(
            "book".into(),
            retry.quote_id,
            true,
            OfflineBudget {
                max_new_blocks: Some(1),
                max_cost_usd: Some("10".into()),
                allow_unknown_cost: false,
            },
        )
        .await
        .unwrap();
    let status = f.wait_stopped().await;
    assert_eq!(status.state, OfflineJobState::Complete);
    assert_eq!(status.submitted_requests, 2);
    assert_eq!(
        status.actual_cost_usd.as_deref(),
        Some("0.300000000000000003")
    );
    f.manager.begin_shutdown();
    timeout(Duration::from_secs(2), f.manager.shutdown())
        .await
        .unwrap();
    let cold = OfflineManager::new(Arc::clone(&f.manager.store), f.root.clone()).unwrap();
    let status = cold.status("book").await.unwrap();
    assert_eq!(status.submitted_requests, 2);
    assert_eq!(
        status.actual_cost_usd.as_deref(),
        Some("0.300000000000000003")
    );
}

#[tokio::test]
async fn filled_cache_does_not_mark_running_job_complete_before_cost_reconciliation() {
    let f = Fixture::new(&["Запрос ещё выполняется."]);
    f.provider.hold();
    let quote = f.quote().await;
    let (block, _) = f.first_block(&quote).await;
    f.start(&quote, Some(1), Some("10"), false).await.unwrap();
    timeout(Duration::from_secs(2), f.provider.wait_calls(1))
        .await
        .unwrap();
    f.manager
        .cache
        .put(&block.id, "audio/mpeg", b"cached")
        .unwrap();
    let status = f.manager.status("book").await.unwrap();
    assert_eq!(status.cached_blocks, 1);
    assert_eq!(status.state, OfflineJobState::Running);
    f.provider.release();
    assert_eq!(f.wait_stopped().await.state, OfflineJobState::Complete);
}

#[tokio::test]
async fn estimated_reservation_precedes_dispatch_and_is_replaced_by_actual_cost() {
    let f = Fixture::new(&["Точная стоимость."]);
    f.provider.set_price("0.000000000000000001", "0", false);
    f.provider.push_usage(Some("0.000000000000000002"));
    f.provider.hold();
    let quote = f.quote().await;
    let estimate = quote.estimated_cost.clone().unwrap();
    f.start(&quote, Some(1), Some(&estimate), false)
        .await
        .unwrap();
    timeout(Duration::from_secs(2), f.provider.wait_calls(1))
        .await
        .unwrap();
    let active = f.manager.status("book").await.unwrap();
    assert_eq!(
        active.estimated_reserved_usd.as_deref(),
        Some(estimate.as_str())
    );
    assert_eq!(active.submitted_requests, 1);
    f.provider.release();
    let done = f.wait_stopped().await;
    assert_eq!(done.state, OfflineJobState::Complete);
    assert_eq!(done.estimated_reserved_usd, None);
    assert_eq!(
        done.actual_cost_usd.as_deref(),
        Some("0.000000000000000002")
    );
}
