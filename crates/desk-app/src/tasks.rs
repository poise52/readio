use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Debug, Default)]
struct State {
    closed: bool,
    active: usize,
}

#[derive(Debug)]
pub struct Tasks {
    state: Mutex<State>,
    active: watch::Sender<usize>,
}

#[derive(Debug)]
pub struct TaskPermit {
    tasks: Arc<Tasks>,
}

impl Tasks {
    pub(crate) fn new() -> Arc<Self> {
        let (active, _) = watch::channel(0);
        Arc::new(Self {
            state: Mutex::new(State::default()),
            active,
        })
    }

    pub(crate) fn enter(self: &Arc<Self>) -> Option<TaskPermit> {
        let mut state = self.state.lock().ok()?;
        if state.closed {
            return None;
        }
        state.active = state.active.checked_add(1)?;
        self.active.send_replace(state.active);
        drop(state);
        Some(TaskPermit {
            tasks: Arc::clone(self),
        })
    }

    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
    }

    pub(crate) async fn wait(&self) {
        let mut active = self.active.subscribe();
        let _ = active.wait_for(|count| *count == 0).await;
    }
}

impl Drop for TaskPermit {
    fn drop(&mut self) {
        let mut state = self
            .tasks
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active -= 1;
        self.tasks.active.send_replace(state.active);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_waits_for_all_tasks_and_rejects_new_work() {
        let tasks = Tasks::new();
        let first = tasks.enter().unwrap();
        let second = tasks.enter().unwrap();
        tasks.close();
        assert!(tasks.enter().is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), tasks.wait())
                .await
                .is_err()
        );
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), tasks.wait())
                .await
                .is_err()
        );
        drop(second);
        tokio::join!(tasks.wait(), tasks.wait());
    }

    #[tokio::test]
    async fn task_panics_release_the_drain_permit() {
        let tasks = Tasks::new();
        let permit = tasks.enter().unwrap();
        let task = tokio::spawn(async move {
            let _permit = permit;
            panic!("simulated worker failure");
        });
        tasks.close();
        assert!(task.await.is_err());
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .unwrap();
    }
}
