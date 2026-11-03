use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

#[derive(Debug, Default)]
pub struct WorkQueue {
    state: Mutex<QueueState>,
}

#[derive(Debug, Default)]
struct QueueState {
    closed: bool,
    playback: Option<Playback>,
    offline: HashMap<String, Offline>,
    submitted: HashSet<String>,
    submitted_books: HashMap<String, HashSet<String>>,
}

#[derive(Debug)]
struct Playback {
    session: String,
    revision: Option<u64>,
    wanted: HashSet<String>,
}

#[derive(Debug)]
struct Offline {
    generation: u64,
    sequence: Option<u64>,
    active: bool,
    wanted: HashSet<String>,
}

impl QueueState {
    fn needs(&self, block: &str) -> bool {
        !self.closed
            && (self
                .playback
                .as_ref()
                .is_some_and(|playback| playback.wanted.contains(block))
                || self
                    .offline
                    .values()
                    .any(|offline| offline.wanted.contains(block)))
    }
}

impl WorkQueue {
    pub(crate) fn open_playback_session(&self) -> Option<String> {
        let mut state = self.state.lock().ok()?;
        if state.closed {
            return None;
        }
        let session = uuid::Uuid::new_v4().to_string();
        state.playback = Some(Playback {
            session: session.clone(),
            revision: None,
            wanted: HashSet::new(),
        });
        drop(state);
        Some(session)
    }

    pub(crate) fn update_playback(
        &self,
        session: &str,
        revision: u64,
        wanted: HashSet<String>,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.closed {
            return false;
        }
        let Some(playback) = state.playback.as_mut() else {
            return false;
        };
        if playback.session != session || playback.revision.is_some_and(|old| old >= revision) {
            return false;
        }
        playback.revision = Some(revision);
        playback.wanted = wanted;
        true
    }

    pub(crate) fn is_current_playback(&self, session: &str, revision: u64, block: &str) -> bool {
        self.state.lock().is_ok_and(|state| {
            !state.closed
                && state.playback.as_ref().is_some_and(|playback| {
                    playback.session == session
                        && playback.revision == Some(revision)
                        && playback.wanted.contains(block)
                })
        })
    }

    pub(crate) fn start_offline(&self, book: &str, generation: u64) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.closed
            || state
                .offline
                .get(book)
                .is_some_and(|offline| offline.generation >= generation)
        {
            return false;
        }
        state.offline.insert(
            book.to_owned(),
            Offline {
                generation,
                sequence: None,
                active: true,
                wanted: HashSet::new(),
            },
        );
        true
    }

    pub(crate) fn update_offline(
        &self,
        book: &str,
        generation: u64,
        sequence: u64,
        wanted: HashSet<String>,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.closed {
            return false;
        }
        let Some(offline) = state.offline.get_mut(book) else {
            return false;
        };
        if !offline.active
            || offline.generation != generation
            || offline.sequence.is_some_and(|old| old >= sequence)
        {
            return false;
        }
        offline.sequence = Some(sequence);
        offline.wanted.clone_from(&wanted);
        for block in wanted {
            if let Some(books) = state.submitted_books.get_mut(&block) {
                books.insert(book.to_owned());
            }
        }
        true
    }

    pub(crate) fn stop_offline(&self, book: &str, generation: u64) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(offline) = state.offline.get_mut(book) else {
            return false;
        };
        if !offline.active || offline.generation != generation {
            return false;
        }
        offline.active = false;
        offline.wanted.clear();
        true
    }

    pub(crate) fn needs(&self, block: &str) -> bool {
        self.state.lock().is_ok_and(|state| state.needs(block))
    }

    pub(crate) fn try_submit(&self, block: &str) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.submitted.contains(block) || !state.needs(block) {
            return false;
        }
        let books = state
            .offline
            .iter()
            .filter(|(_, job)| job.active && job.wanted.contains(block))
            .map(|(book, _)| book.clone())
            .collect();
        state.submitted_books.insert(block.to_owned(), books);
        state.submitted.insert(block.to_owned());
        true
    }

    pub(crate) fn complete(&self, block: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.submitted.remove(block);
            state.submitted_books.remove(block);
        }
    }
    pub(crate) fn pending_for_book(&self, book: &str) -> usize {
        self.state.lock().map_or(0, |state| {
            state
                .submitted_books
                .values()
                .filter(|books| books.contains(book))
                .count()
        })
    }
    pub(crate) fn submitted_books(&self, block: &str) -> Vec<String> {
        self.state.lock().map_or_else(
            |_| Vec::new(),
            |state| {
                state
                    .submitted_books
                    .get(block)
                    .map_or_else(Vec::new, |books| books.iter().cloned().collect())
            },
        )
    }

    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.playback = None;
            for offline in state.offline.values_mut() {
                offline.active = false;
                offline.wanted.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn blocks(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn jumps_while_gate_is_held_only_submit_latest() {
        let queue = Arc::new(WorkQueue::default());
        let session = queue.open_playback_session().unwrap();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let ready = Arc::new(Barrier::new(21));
        let mut workers = Vec::new();
        for revision in 0..20 {
            let id = format!("block-{revision}");
            assert!(queue.update_playback(&session, revision, HashSet::from([id.clone()])));
            let queue = Arc::clone(&queue);
            let gate = Arc::clone(&gate);
            let ready = Arc::clone(&ready);
            workers.push(thread::spawn(move || {
                ready.wait();
                let _gate = gate.lock().unwrap();
                (id.clone(), queue.try_submit(&id))
            }));
        }
        ready.wait();
        drop(held);
        let submitted: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|(_, submitted)| *submitted)
            .map(|(id, _)| id)
            .collect();
        assert_eq!(submitted, ["block-19"]);
    }

    #[test]
    fn stale_revisions_and_replaced_sessions_cannot_restore_interest() {
        let queue = WorkQueue::default();
        let session = queue.open_playback_session().unwrap();
        assert!(queue.update_playback(&session, 0, blocks(&["first"])));
        assert!(queue.update_playback(&session, 2, blocks(&["last", "next"])));
        assert!(!queue.update_playback(&session, 1, blocks(&["first"])));
        assert!(!queue.update_playback(&session, 2, blocks(&["first"])));
        assert!(!queue.is_current_playback(&session, 1, "first"));
        assert!(queue.is_current_playback(&session, 2, "last"));
        let replacement = queue.open_playback_session().unwrap();
        assert_ne!(session, replacement);
        assert!(!queue.needs("last"));
        assert!(!queue.update_playback(&session, 3, blocks(&["first"])));
        assert!(queue.update_playback(&replacement, 0, blocks(&["new"])));
        assert!(queue.try_submit("new"));
    }

    #[test]
    fn offline_generation_and_pause_reject_late_updates() {
        let queue = WorkQueue::default();
        assert!(queue.start_offline("book", 1));
        assert!(queue.update_offline("book", 1, 1, blocks(&["old"])));
        assert!(queue.stop_offline("book", 1));
        assert!(!queue.update_offline("book", 1, 2, blocks(&["old"])));
        assert!(!queue.try_submit("old"));
        assert!(queue.start_offline("book", 2));
        assert!(queue.update_offline("book", 2, 0, blocks(&["new"])));
        assert!(!queue.stop_offline("book", 1));
        assert!(!queue.update_offline("book", 1, u64::MAX, blocks(&["old"])));
        assert!(!queue.update_offline("book", 2, 0, blocks(&["old"])));
        assert!(queue.try_submit("new"));
    }

    #[test]
    fn independent_consumers_keep_shared_work_eligible() {
        let queue = WorkQueue::default();
        let session = queue.open_playback_session().unwrap();
        assert!(queue.update_playback(&session, 1, blocks(&["shared"])));
        assert!(queue.start_offline("book", 1));
        assert!(queue.update_offline("book", 1, 1, blocks(&["shared"])));
        assert!(queue.update_playback(&session, 2, blocks(&[])));
        assert!(queue.needs("shared"));
        assert!(queue.try_submit("shared"));
        assert!(queue.stop_offline("book", 1));
        assert!(!queue.needs("shared"));
        queue.complete("shared");
        assert!(!queue.try_submit("shared"));
    }

    #[test]
    fn submitted_work_drains_after_revoke_and_close_blocks_new_work() {
        let queue = WorkQueue::default();
        let session = queue.open_playback_session().unwrap();
        assert!(queue.update_playback(&session, 1, blocks(&["active", "queued"])));
        assert!(queue.try_submit("active"));
        assert!(queue.update_playback(&session, 2, blocks(&[])));
        assert!(!queue.try_submit("queued"));
        assert!(queue.update_playback(&session, 3, blocks(&["active"])));
        assert!(!queue.try_submit("active"));
        queue.close();
        assert!(queue.open_playback_session().is_none());
        assert!(!queue.update_playback(&session, 4, blocks(&["queued"])));
        assert!(!queue.start_offline("book", 1));
        queue.complete("active");
        assert!(!queue.try_submit("active"));
    }
}
