#![allow(clippy::unwrap_used)]

use super::*;
use desk_core::Chapter;

#[tokio::test]
async fn verifying_cached_audio_does_not_lock_generation_journal() {
    let root = std::env::temp_dir().join(format!("readio-quote-lock-{}", Uuid::new_v4()));
    let store = Arc::new(std::sync::Mutex::new(Store::open(&root).unwrap()));
    let mut book = Book {
        id: "quote-lock".into(),
        title: "Тест".into(),
        chapters: vec![Chapter {
            title: "Глава".into(),
            text: "Одна фраза.".into(),
        }],
        ..Book::default()
    };
    book.refresh();
    store.lock().unwrap().import_parsed(&[book]).unwrap();
    let manager = OfflineManager::new(Arc::clone(&store), root.clone()).unwrap();
    let quote = manager.quote("quote-lock".into()).await.unwrap();
    let (plan, profile) = {
        let quotes = manager.quotes.lock().await;
        let quote = quotes.get(&quote.quote_id).unwrap();
        let result = (Arc::clone(&quote.plan), quote.manifest.profile.clone());
        drop(quotes);
        result
    };
    let journal = Arc::clone(&manager.journal);
    let scan_journal = Arc::clone(&journal);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut started = Some(started_tx);
    let scan = tauri::async_runtime::spawn_blocking(move || {
        scan_quote(&plan, &profile, &scan_journal, |_| {
            if let Some(started) = started.take() {
                started.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Ok(true)
        })
    });
    started_rx.await.unwrap();
    assert!(
        journal.try_lock().is_ok(),
        "journal locked during cache verification"
    );
    release_tx.send(()).unwrap();
    scan.await.unwrap().unwrap();
    manager.shutdown().await;
    drop(manager);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn completed_job_does_not_keep_edition_plan_after_cache_invalidation() {
    let root = std::env::temp_dir().join(format!("readio-memory-test-{}", Uuid::new_v4()));
    let store = Arc::new(std::sync::Mutex::new(Store::open(&root).unwrap()));
    let mut book = Book {
        id: "memory-test".into(),
        title: "Тест".into(),
        author: "Автор".into(),
        chapters: vec![Chapter {
            title: "Глава".into(),
            text: "Одна фраза. Другая фраза!".into(),
        }],
        ..Book::default()
    };
    book.refresh();
    store.lock().unwrap().import_parsed(&[book]).unwrap();
    let manager = OfflineManager::new(Arc::clone(&store), root.clone()).unwrap();
    let quote = manager.quote("memory-test".into()).await.unwrap();
    let (weak, ids) = {
        let quotes = manager.quotes.lock().await;
        let quote = quotes.get(&quote.quote_id).unwrap();
        let result = (
            Arc::downgrade(&quote.plan),
            quote.manifest.block_ids.clone(),
        );
        drop(quotes);
        result
    };
    for id in ids {
        manager.cache.put(&id, "audio/mpeg", b"cached").unwrap();
    }
    manager
        .start(
            "memory-test".into(),
            quote.quote_id,
            false,
            OfflineBudget::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), manager.tasks.wait())
        .await
        .unwrap();
    assert_eq!(
        manager.jobs.lock().await.get("memory-test").unwrap().state,
        OfflineJobState::Complete
    );
    manager.remove_all("memory-test").await.unwrap();
    let released = tokio::time::timeout(Duration::from_secs(10), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(released.is_ok(), "completed job retained an edition plan");
    manager.shutdown().await;
    drop(manager);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
