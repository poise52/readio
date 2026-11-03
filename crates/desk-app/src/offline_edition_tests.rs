#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use desk_core::{Chapter, plan_chapter_v1};
use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf, time::Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    manager: Option<Arc<OfflineManager>>,
    root: PathBuf,
}

impl Fixture {
    fn new(provider: Option<Arc<crate::test_provider::MockSpeech>>) -> Self {
        let root = std::env::temp_dir().join(format!("readio-edition-test-{}", Uuid::new_v4()));
        let store = Arc::new(std::sync::Mutex::new(Store::open(&root).unwrap()));
        let mut manager = OfflineManager::new(store, root.clone()).unwrap();
        Arc::get_mut(&mut manager).unwrap().test_provider = provider;
        Self {
            manager: Some(manager),
            root,
        }
    }

    fn manager(&self) -> &Arc<OfflineManager> {
        self.manager.as_ref().unwrap()
    }

    fn insert(&self, book: &Book) {
        self.manager()
            .store
            .lock()
            .unwrap()
            .import_parsed(std::slice::from_ref(book))
            .unwrap();
    }

    fn cold_manager(&self) -> Arc<OfflineManager> {
        OfflineManager::new(Arc::clone(&self.manager().store), self.root.clone()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.manager.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn book(id: &str, chapters: &[&str]) -> Book {
    let mut book = Book {
        id: id.into(),
        title: "Тест".into(),
        author: "Автор".into(),
        chapters: chapters
            .iter()
            .enumerate()
            .map(|(index, text)| Chapter {
                title: format!("Глава {index}"),
                text: (*text).into(),
            })
            .collect(),
        ..Book::default()
    };
    book.refresh();
    book
}

fn openrouter_profile() -> AudioProfile {
    AudioProfile {
        engine: "openrouter".into(),
        voice: "openrouter".into(),
        model: Some("test-model".into()),
        model_voice: None,
        dictionary: BTreeMap::new(),
        max_escaped_bytes: Some(100),
        synthesis: None,
    }
}

fn persist(manager: &OfflineManager, manifest: &OfflineManifest, plan: &EditionPlan) {
    let digest = manager.cache.save_edition_plan(plan).unwrap();
    assert_eq!(manifest.plan_digest.as_deref(), Some(digest.as_str()));
    manager.cache.save_manifest(manifest).unwrap();
}

#[tokio::test]
async fn chapter_plan_is_shared_for_twenty_concurrent_calls_and_replaced_after_delete() -> TestResult
{
    let fixture = Fixture::new(None);
    let huge = "другой текст ".repeat(160_000);
    let original = book("same-id", &["😀 Первая фраза. Конец!", &huge]);
    fixture.insert(&original);
    let manager = fixture.manager();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let manager = Arc::clone(manager);
        tasks.spawn(async move { manager.chapter_plan("same-id".into(), 0).await });
    }
    let mut plans = Vec::new();
    while let Some(result) = tasks.join_next().await {
        plans.push(result??.1);
    }
    let first = plans.first().unwrap();
    assert!(plans.iter().all(|plan| Arc::ptr_eq(first, plan)));
    assert_eq!(manager.chapter_builds.load(Ordering::SeqCst), 1);
    assert_eq!(first.blocks[0].start, 0);
    assert_eq!(
        first.blocks.last().unwrap().end,
        original.chapters[0].text.encode_utf16().count()
    );
    assert_eq!(
        first.block(&first.blocks[0].id).unwrap().text,
        first.blocks[0].text
    );
    let old_ids: Vec<_> = first.blocks.iter().map(|block| block.id.clone()).collect();
    {
        let store = manager.store.lock().unwrap();
        manager.invalidate_plans();
        store.delete("same-id")?;
        store.import_parsed(&[book("same-id", &["Новая версия. 😀"])])?;
    }
    let (_, second) = manager.chapter_plan("same-id".into(), 0).await?;
    assert!(!Arc::ptr_eq(first, &second));
    assert_ne!(
        old_ids,
        second
            .blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(manager.chapter_builds.load(Ordering::SeqCst), 2);
    let (_, warm) = manager.chapter_plan("same-id".into(), 0).await?;
    assert!(Arc::ptr_eq(&second, &warm));
    Ok(())
}

#[tokio::test]
async fn legacy_edition_migrates_without_network_and_keeps_existing_audio() -> TestResult {
    let fixture = Fixture::new(None);
    let source = book(
        "legacy-id",
        &["😀 Один текст. Второй текст!", "Другая глава."],
    );
    fixture.insert(&source);
    let manager = fixture.manager();
    let profile = openrouter_profile();
    let ids: Vec<_> = (0..source.chapters.len())
        .flat_map(|index| plan_chapter_v1(&source, index, &profile))
        .map(|block| block.id)
        .collect();
    manager.cache.put(&ids[0], "audio/mpeg", b"saved-audio")?;
    let legacy = OfflineManifest {
        book_id: source.id.clone(),
        profile_id: profile.id(),
        profile,
        block_ids: ids.clone(),
        plan_digest: None,
        updated_at: 0,
    };
    manager.cache.save_manifest(&legacy)?;
    let cold = fixture.cold_manager();
    let (saved, plan) = cold.saved_edition(source.id.clone()).await?;
    assert_eq!(saved.block_ids, ids);
    assert_eq!(plan.block_ids(), ids);
    let digest = saved
        .plan_digest
        .as_deref()
        .expect("migration persisted digest");
    assert_eq!(
        cold.cache
            .manifest(&source.id, &legacy.profile_id)?
            .unwrap()
            .plan_digest
            .as_deref(),
        Some(digest)
    );
    assert_eq!(cold.cache.edition_plan(digest)?.block_ids(), ids);
    assert!(cold.cache.get(&ids[0])?.is_some());
    assert_eq!(cold.chapter_builds.load(Ordering::SeqCst), 0);
    drop(cold);
    Ok(())
}

#[tokio::test]
async fn removal_precedes_queued_legacy_migration_without_resurrection() -> TestResult {
    use std::{future::Future, task::Poll};

    let fixture = Fixture::new(None);
    let source = book("legacy-race", &["Текст старого издания. 😀"]);
    fixture.insert(&source);
    let manager = fixture.manager();
    let profile = openrouter_profile();
    let ids = plan_chapter_v1(&source, 0, &profile)
        .into_iter()
        .map(|block| block.id)
        .collect();
    let legacy = OfflineManifest {
        book_id: source.id.clone(),
        profile_id: profile.id(),
        profile,
        block_ids: ids,
        plan_digest: None,
        updated_at: 0,
    };
    manager.cache.save_manifest(&legacy)?;

    let gate = manager.plan_gate.lock().await;
    let epoch = manager.edition_plans.epoch();
    let mut remove = std::pin::pin!(manager.remove_all(&source.id));
    let mut migration = std::pin::pin!(manager.edition(legacy.clone()));
    std::future::poll_fn(|context| {
        assert!(remove.as_mut().poll(context).is_pending());
        assert_eq!(manager.edition_plans.epoch(), epoch);
        assert!(migration.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(gate);

    let (removed, migrated) = tokio::join!(remove, migration);
    removed?;
    assert!(
        migrated.is_err(),
        "a stale legacy manifest must not be restored"
    );
    assert!(
        manager
            .cache
            .manifest(&source.id, &legacy.profile_id)?
            .is_none()
    );
    assert!(manager.store.lock().unwrap().book(&source.id).is_ok());
    Ok(())
}

#[tokio::test]
async fn saved_edition_and_worker_keep_snapshot_boundaries_and_remove_does_not_resurrect()
-> TestResult {
    let mock = crate::test_provider::MockSpeech::new();
    let fixture = Fixture::new(Some(Arc::clone(&mock)));
    let mut content = String::new();
    for index in 0..25 {
        write!(content, "Фраза {index} с уникальным содержимым. ").unwrap();
    }
    let source = book("snapshot-id", &[&content]);
    fixture.insert(&source);
    let manager = fixture.manager();
    let profile = openrouter_profile();
    let (manifest, plan) = manager.new_edition(source.clone(), profile.clone()).await?;
    let digest = manifest.plan_digest.as_ref().unwrap();
    assert!(
        manager.cache.edition_plan(digest).is_err(),
        "quote must not persist a plan"
    );
    let saved_ids = plan.block_ids();
    let mut current_profile = profile.clone();
    current_profile.max_escaped_bytes = None;
    let current_blocks = desk_core::plan_chapter(&source, 0, &current_profile);
    let saved_ranges: Vec<_> = plan.chapters[0]
        .blocks
        .iter()
        .map(|block| (block.start, block.end))
        .collect();
    let current_ranges: Vec<_> = current_blocks
        .iter()
        .map(|block| (block.start, block.end))
        .collect();
    assert_ne!(saved_ranges, current_ranges);
    persist(manager, &manifest, &plan);
    let cold = fixture.cold_manager();
    let (saved, loaded) = cold.saved_edition(source.id.clone()).await?;
    assert_eq!(loaded.block_ids(), saved_ids);
    assert_eq!(saved.profile.max_escaped_bytes, Some(100));
    assert_eq!(cold.chapter_builds.load(Ordering::SeqCst), 0);
    drop(cold);

    assert!(manager.queue.start_offline(&source.id, 1));
    manager.jobs.lock().await.insert(
        source.id.clone(),
        Job {
            state: OfflineJobState::Running,
            error: None,
            generation: 1,
            engine: profile.engine.clone(),
            budget: None,
        },
    );
    let allowed = saved_ids.iter().cloned().collect::<HashSet<_>>();
    Arc::clone(manager)
        .worker(OfflineWork {
            book_id: source.id.clone(),
            plan: Arc::clone(&plan),
            profile,
            generation: 1,
            started_at: Instant::now(),
            allowed: allowed.clone(),
            retry_attempts: HashMap::new(),
        })
        .await;
    let calls = mock.calls();
    assert_eq!(calls.len(), allowed.len());
    let actual: HashSet<_> = calls.into_iter().collect();
    assert_eq!(actual, allowed);
    assert_eq!(
        manager.jobs.lock().await.get(&source.id).unwrap().state,
        OfflineJobState::Complete
    );
    assert_eq!(manager.chapter_builds.load(Ordering::SeqCst), 0);
    let _ = manager.saved_edition(source.id.clone()).await?;
    manager.remove_all(&source.id).await?;
    assert!(manager.saved_edition(source.id.clone()).await.is_err());
    Ok(())
}

#[tokio::test]
async fn five_megabyte_chapter_reuses_warm_plan() -> TestResult {
    let fixture = Fixture::new(None);
    let text = "а".repeat(5 * 1024 * 1024 / 2);
    let source = book("benchmark-id", &[&text]);
    fixture.insert(&source);
    let manager = fixture.manager();
    let start = Instant::now();
    let (_, cold) = manager.chapter_plan(source.id.clone(), 0).await?;
    let cold_time = start.elapsed();
    let start = Instant::now();
    let (_, warm) = manager.chapter_plan(source.id.clone(), 0).await?;
    let warm_time = start.elapsed();
    assert!(Arc::ptr_eq(&cold, &warm));
    assert_eq!(manager.chapter_builds.load(Ordering::SeqCst), 1);
    assert_eq!(
        cold.blocks
            .iter()
            .map(|block| &block.id)
            .collect::<Vec<_>>(),
        warm.blocks
            .iter()
            .map(|block| &block.id)
            .collect::<Vec<_>>()
    );
    let reader_bytes =
        serde_json::to_vec(&manager.store.lock().unwrap().reader_book(&source.id)?)?.len();
    eprintln!(
        "5 MiB TXT: cold={cold_time:?}, warm={warm_time:?}, reader IPC={reader_bytes} bytes, blocks={}",
        cold.blocks.len()
    );
    Ok(())
}
