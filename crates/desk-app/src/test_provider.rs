#![allow(clippy::unwrap_used)]

use readio_speech::{SpeechAudio, SpeechError};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Semaphore, watch};

#[derive(Debug)]
pub struct MockSpeech {
    calls: Mutex<Vec<String>>,
    count: watch::Sender<usize>,
    hold: AtomicBool,
    released: Semaphore,
}

impl MockSpeech {
    pub(crate) fn new() -> Arc<Self> {
        let (count, _) = watch::channel(0);
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            count,
            hold: AtomicBool::new(false),
            released: Semaphore::new(0),
        })
    }

    pub(crate) fn hold(&self) {
        self.hold.store(true, Ordering::Release);
    }

    pub(crate) fn release(&self) {
        self.released.add_permits(1);
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) async fn wait_calls(&self, count: usize) {
        let mut rx = self.count.subscribe();
        rx.wait_for(|current| *current >= count).await.unwrap();
    }

    pub(crate) async fn synthesize<F, Fut>(
        &self,
        block_id: &str,
        on_response: F,
    ) -> Result<SpeechAudio, SpeechError>
    where
        F: FnOnce(Option<String>) -> Fut + Send,
        Fut: Future<Output = Result<(), SpeechError>> + Send,
    {
        {
            let mut calls = self.calls.lock().unwrap();
            calls.push(block_id.to_owned());
            self.count.send_replace(calls.len());
        }
        on_response(Some("gen-test".into())).await?;
        if self.hold.load(Ordering::Acquire) {
            self.released
                .acquire()
                .await
                .map_err(|_| SpeechError::Cancelled)?
                .forget();
        }
        Ok(SpeechAudio {
            data: "bXAzLXRlc3Q=".into(),
            mime: "audio/mpeg".into(),
            playback_rate: 1.0,
        })
    }
}
