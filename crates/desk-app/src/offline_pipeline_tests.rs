#![allow(clippy::unwrap_used)]

use super::*;
use crate::test_provider::MockSpeech;
use std::{collections::BTreeMap, path::PathBuf};
use tokio::time::timeout;

struct TestDir(PathBuf);
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup() -> (TestDir, Arc<OfflineManager>, Arc<MockSpeech>) {
    let root = TestDir(std::env::temp_dir().join(format!("readio-pipeline-{}", Uuid::new_v4())));
    let store = Arc::new(std::sync::Mutex::new(Store::open(&root.0).unwrap()));
    let mut manager = OfflineManager::new(store, root.0.clone()).unwrap();
    let provider = MockSpeech::new();
    Arc::get_mut(&mut manager).unwrap().test_provider = Some(Arc::clone(&provider));
    (root, manager, provider)
}

fn profile() -> AudioProfile {
    AudioProfile {
        engine: "openrouter".into(),
        voice: "openrouter".into(),
        model: Some("test/model".into()),
        model_voice: None,
        dictionary: BTreeMap::new(),
        max_escaped_bytes: None,
        synthesis: None,
    }
}

fn block(n: u64) -> AudioBlock {
    AudioBlock {
        id: format!("{n:064x}"),
        chapter: 0,
        start: 0,
        end: 4,
        passages: Vec::new(),
        text: format!("test block {n}"),
        characters: 4,
    }
}

async fn wait_inflight(manager: &OfflineManager, id: &str) {
    timeout(Duration::from_secs(2), async {
        loop {
            if manager.inflight.lock().await.contains_key(id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn submit(
    manager: &Arc<OfflineManager>,
    block: AudioBlock,
    profile: AudioProfile,
    job: Option<(String, u64)>,
    retry: Option<Instant>,
    consent: Option<String>,
) -> tokio::task::JoinHandle<BlockResult<Option<SpeechAudio>>> {
    let manager = Arc::clone(manager);
    tokio::spawn(async move {
        manager
            .request_block(block, profile, job, retry, consent)
            .await
    })
}

#[tokio::test]
async fn twenty_seeks_dispatch_only_the_current_block() {
    let (_root, manager, provider) = setup();
    let session = manager.queue.open_playback_session().unwrap();
    let gate = manager.gate.lock().await;
    let mut pending = Vec::new();
    for n in 0..20 {
        let block = block(n);
        assert!(
            manager
                .queue
                .update_playback(&session, n, HashSet::from([block.id.clone()]))
        );
        let id = block.id.clone();
        pending.push(submit(&manager, block, profile(), None, None, None));
        wait_inflight(&manager, &id).await;
    }
    drop(gate);
    for worker in pending {
        let _ = timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(provider.calls(), vec![block(19).id]);
    assert!(manager.cache.verify(&block(19).id).unwrap().is_some());
}

#[tokio::test]
async fn shared_interest_survives_playback_revoke_and_coalesces() {
    let (_root, manager, provider) = setup();
    let block = block(31);
    let session = manager.queue.open_playback_session().unwrap();
    assert!(
        manager
            .queue
            .update_playback(&session, 1, HashSet::from([block.id.clone()]))
    );
    assert!(manager.queue.start_offline("book", 1));
    assert!(
        manager
            .queue
            .update_offline("book", 1, 1, HashSet::from([block.id.clone()]))
    );
    let gate = manager.gate.lock().await;
    let a = submit(&manager, block.clone(), profile(), None, None, None);
    wait_inflight(&manager, &block.id).await;
    let b = submit(
        &manager,
        block.clone(),
        profile(),
        Some(("book".into(), 1)),
        None,
        None,
    );
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(manager.queue.update_playback(&session, 2, HashSet::new()));
    drop(gate);
    assert!(
        timeout(Duration::from_secs(2), a)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_some()
    );
    assert!(
        timeout(Duration::from_secs(2), b)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_some()
    );
    assert_eq!(provider.calls(), [block.id]);
}

#[tokio::test]
async fn submitted_database_failure_never_contacts_provider() {
    let (root, manager, provider) = setup();
    let db = rusqlite::Connection::open(root.0.join("readio.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_submit BEFORE UPDATE OF state ON generation_attempts WHEN NEW.state='submitted' BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    let block = block(41);
    let session = manager.queue.open_playback_session().unwrap();
    assert!(
        manager
            .queue
            .update_playback(&session, 1, HashSet::from([block.id.clone()]))
    );
    let result = Arc::clone(&manager)
        .request_block(block.clone(), profile(), None, None, None)
        .await;
    assert!(result.is_err());
    assert_eq!(provider.calls(), Vec::<String>::new());
    assert_eq!(
        manager
            .journal
            .lock()
            .unwrap()
            .latest(&block, &profile())
            .unwrap()
            .unwrap()
            .state,
        GenerationState::Prepared
    );
}

#[tokio::test]
async fn cache_after_completion_failure_repairs_journal_without_retrying_provider() {
    let (root, manager, provider) = setup();
    let db = rusqlite::Connection::open(root.0.join("readio.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_complete BEFORE UPDATE OF state ON generation_attempts WHEN NEW.state='completed' BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    let block = block(42);
    let session = manager.queue.open_playback_session().unwrap();
    assert!(
        manager
            .queue
            .update_playback(&session, 1, HashSet::from([block.id.clone()]))
    );
    assert!(
        Arc::clone(&manager)
            .request_block(block.clone(), profile(), None, None, None)
            .await
            .is_err()
    );
    assert!(manager.cache.verify(&block.id).unwrap().is_some());
    assert_eq!(
        manager
            .journal
            .lock()
            .unwrap()
            .latest(&block, &profile())
            .unwrap()
            .unwrap()
            .state,
        GenerationState::Uncertain
    );
    db.execute_batch("DROP TRIGGER fail_complete;").unwrap();
    assert!(
        Arc::clone(&manager)
            .request_block(block.clone(), profile(), None, None, None)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(provider.calls().len(), 1);
    let complete = manager
        .journal
        .lock()
        .unwrap()
        .latest(&block, &profile())
        .unwrap()
        .unwrap();
    assert_eq!(complete.state, GenerationState::Completed);
    std::fs::remove_file(
        root.0
            .join("audio-cache/blocks")
            .join(format!("{}.audio", block.id)),
    )
    .unwrap();
    let result = Arc::clone(&manager)
        .request_block(block.clone(), profile(), None, Some(Instant::now()), None)
        .await
        .unwrap_err();
    assert_eq!(result.code, BlockErrorCode::RetryRequired);
    assert_eq!(result.attempt_id.as_deref(), Some(complete.id.as_str()));
    let retry = Arc::clone(&manager)
        .request_block(
            block.clone(),
            profile(),
            None,
            Some(Instant::now()),
            Some(complete.id),
        )
        .await
        .unwrap();
    assert!(retry.is_some());
    assert_eq!(provider.calls().len(), 2);
}

#[tokio::test]
async fn shutdown_drains_submitted_and_discards_queued_block() {
    let (_root, manager, provider) = setup();
    provider.hold();
    let session = manager.queue.open_playback_session().unwrap();
    let active = block(51);
    let queued = block(52);
    assert!(manager.queue.update_playback(
        &session,
        1,
        HashSet::from([active.id.clone(), queued.id.clone()])
    ));
    let first = submit(&manager, active.clone(), profile(), None, None, None);
    timeout(Duration::from_secs(2), provider.wait_calls(1))
        .await
        .unwrap();
    let queued_id = queued.id.clone();
    let second = submit(&manager, queued, profile(), None, None, None);
    wait_inflight(&manager, &queued_id).await;
    manager.begin_shutdown();
    assert!(
        timeout(Duration::from_millis(50), manager.shutdown())
            .await
            .is_err()
    );
    provider.release();
    timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_some()
    );
    let _ = timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.calls(), std::slice::from_ref(&active.id));
    assert!(manager.cache.verify(&active.id).unwrap().is_some());
}

#[tokio::test]
async fn panic_marks_submitted_uncertain_and_retry_requires_exact_consent() {
    let (_root, manager, provider) = setup();
    let block = block(61);
    let profile = profile();
    let attempt = {
        let journal = manager.journal.lock().unwrap();
        let GenerationPrepare::Ready(attempt) = journal.prepare(&block, &profile, false).unwrap()
        else {
            panic!("expected a prepared attempt");
        };
        journal.mark_submitted(&attempt.id).unwrap();
        attempt
    };
    let panic_result = timeout(
        Duration::from_secs(2),
        manager.coalesce(block.id.clone(), None, async {
            panic!("injected worker panic");
            #[allow(unreachable_code)]
            Ok(None)
        }),
    )
    .await
    .unwrap();
    assert!(panic_result.is_err());
    let recovered = manager
        .journal
        .lock()
        .unwrap()
        .latest(&block, &profile)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, attempt.id);
    assert_eq!(recovered.state, GenerationState::Uncertain);
    let session = manager.queue.open_playback_session().unwrap();
    assert!(
        manager
            .queue
            .update_playback(&session, 1, HashSet::from([block.id.clone()]))
    );
    let refused = Arc::clone(&manager)
        .request_block(
            block.clone(),
            profile.clone(),
            None,
            Some(Instant::now()),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, BlockErrorCode::RetryRequired);
    assert_eq!(refused.attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(provider.calls(), Vec::<String>::new());
    let audio = Arc::clone(&manager)
        .request_block(
            block.clone(),
            profile,
            None,
            Some(Instant::now()),
            Some(attempt.id),
        )
        .await
        .unwrap();
    assert!(audio.is_some());
    assert_eq!(provider.calls(), std::slice::from_ref(&block.id));
    assert!(manager.cache.verify(&block.id).unwrap().is_some());
    manager.begin_shutdown();
    timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .unwrap();
}
