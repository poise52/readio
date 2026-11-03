use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

#[derive(Debug)]
pub struct Lifecycle {
    exit_started: AtomicBool,
    drained: AtomicBool,
    frontend_ready: watch::Sender<bool>,
}

impl Lifecycle {
    pub(crate) fn new() -> Self {
        let (frontend_ready, _) = watch::channel(false);
        Self {
            exit_started: AtomicBool::new(false),
            drained: AtomicBool::new(false),
            frontend_ready,
        }
    }

    pub(crate) fn begin_exit(&self) -> bool {
        self.exit_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn frontend_ready(&self) {
        self.frontend_ready.send_replace(true);
    }

    pub(crate) async fn wait_frontend(&self) {
        let mut ready = self.frontend_ready.subscribe();
        let _ = ready.wait_for(|ready| *ready).await;
    }

    pub(crate) fn mark_drained(&self) {
        self.drained.store(true, Ordering::Release);
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.drained.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn frontend_readiness_is_not_lost_before_or_after_subscribing() {
        let ready = Arc::new(Lifecycle::new());
        ready.frontend_ready();
        tokio::time::timeout(Duration::from_secs(1), ready.wait_frontend())
            .await
            .unwrap();

        let pending = Arc::new(Lifecycle::new());
        let state = Arc::clone(&pending);
        let waiter = tokio::spawn(async move { state.wait_frontend().await });
        tokio::task::yield_now().await;
        pending.frontend_ready();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn exit_is_started_once_and_drained_is_explicit() {
        let lifecycle = Lifecycle::new();
        assert!(lifecycle.begin_exit());
        assert!(!lifecycle.begin_exit());
        assert!(!lifecycle.is_drained());
        lifecycle.mark_drained();
        assert!(lifecycle.is_drained());
    }
}
