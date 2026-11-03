#![allow(clippy::unwrap_used)]

use readio_speech::{SpeechAudio, SpeechError};
use std::{
    collections::{HashMap, VecDeque},
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
    price: Mutex<(String, String, bool)>,
    next_costs: Mutex<VecDeque<Option<String>>>,
    costs: Mutex<HashMap<String, Option<String>>>,
}

impl MockSpeech {
    pub(crate) fn new() -> Arc<Self> {
        let (count, _) = watch::channel(0);
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            count,
            hold: AtomicBool::new(false),
            released: Semaphore::new(0),
            price: Mutex::new(("0.01".into(), "0".into(), false)),
            next_costs: Mutex::new(VecDeque::new()),
            costs: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn set_price(&self, prompt: &str, completion: &str, free: bool) {
        *self.price.lock().unwrap() = (prompt.into(), completion.into(), free);
    }
    pub(crate) fn price(&self) -> (String, String, bool) {
        self.price.lock().unwrap().clone()
    }
    pub(crate) fn push_usage(&self, value: Option<&str>) {
        self.next_costs
            .lock()
            .unwrap()
            .push_back(value.map(str::to_owned));
    }
    pub(crate) fn set_usage(&self, id: &str, value: Option<&str>) {
        self.costs
            .lock()
            .unwrap()
            .insert(id.into(), value.map(str::to_owned));
    }
    pub(crate) fn usage(&self, id: &str) -> Result<String, String> {
        self.costs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .flatten()
            .ok_or_else(|| "Расход ещё неизвестен".into())
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
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(block_id.to_owned());
            calls.len()
        };
        let id = format!("gen-test-{count}");
        let cost = self
            .next_costs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Some("0".into()));
        self.costs.lock().unwrap().insert(id.clone(), cost);
        on_response(Some(id)).await?;
        self.count.send_replace(count);
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
            timings: Vec::new(),
        })
    }
}
