use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

const MAX_ENTRIES: usize = 128;
const DISABLED_EPOCH: u64 = u64::MAX;

#[derive(Debug)]
struct Entry<T> {
    value: Arc<T>,
    bytes: usize,
}

#[derive(Debug)]
struct State<T> {
    epoch: u64,
    used: usize,
    entries: HashMap<String, Entry<T>>,
    lru: VecDeque<String>,
}

#[derive(Debug)]
pub struct PlanCache<T> {
    max_bytes: usize,
    state: Mutex<State<T>>,
}

impl<T> PlanCache<T> {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: Mutex::new(State {
                epoch: 0,
                used: 0,
                entries: HashMap::new(),
                lru: VecDeque::new(),
            }),
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<Arc<T>> {
        let mut state = self.state.lock().ok()?;
        if state.epoch == DISABLED_EPOCH {
            return None;
        }
        let value = Arc::clone(&state.entries.get(key)?.value);
        if let Some(index) = state.lru.iter().position(|item| item == key) {
            state.lru.remove(index);
        }
        state.lru.push_back(key.to_owned());
        drop(state);
        Some(value)
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.state
            .lock()
            .map_or(DISABLED_EPOCH, |state| state.epoch)
    }

    pub(crate) fn insert_if_epoch(
        &self,
        epoch: u64,
        key: String,
        value: Arc<T>,
        bytes: usize,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.epoch != epoch || epoch == DISABLED_EPOCH || bytes > self.max_bytes {
            return false;
        }
        if let Some(old) = state.entries.remove(&key) {
            state.used -= old.bytes;
            if let Some(index) = state.lru.iter().position(|item| item == &key) {
                state.lru.remove(index);
            }
        }
        while state.entries.len() >= MAX_ENTRIES || state.used > self.max_bytes - bytes {
            let Some(old_key) = state.lru.pop_front() else {
                return false;
            };
            if let Some(old) = state.entries.remove(&old_key) {
                state.used -= old.bytes;
            }
        }
        state.used += bytes;
        state.lru.push_back(key.clone());
        state.entries.insert(key, Entry { value, bytes });
        true
    }

    pub(crate) fn insert(&self, key: String, value: Arc<T>, bytes: usize) -> bool {
        self.insert_if_epoch(self.epoch(), key, value, bytes)
    }

    pub(crate) fn invalidate(&self) -> u64 {
        let Ok(mut state) = self.state.lock() else {
            return DISABLED_EPOCH;
        };
        state.entries.clear();
        state.lru.clear();
        state.used = 0;
        state.epoch = state.epoch.checked_add(1).unwrap_or(DISABLED_EPOCH);
        state.epoch
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        self.invalidate();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_by_bytes_and_keeps_the_recently_read_entry() {
        let cache = PlanCache::new(10);
        assert!(cache.insert("a".into(), Arc::new(1), 4));
        assert!(cache.insert("b".into(), Arc::new(2), 4));
        assert_eq!(*cache.get("a").unwrap(), 1);
        assert!(cache.insert("c".into(), Arc::new(3), 4));
        assert!(cache.get("b").is_none());
        assert_eq!(*cache.get("a").unwrap(), 1);
        assert_eq!(*cache.get("c").unwrap(), 3);
    }

    #[test]
    fn entry_limit_applies_even_to_zero_sized_values() {
        let cache = PlanCache::new(0);
        for i in 0..=MAX_ENTRIES {
            assert!(cache.insert(i.to_string(), Arc::new(i), 0));
        }
        assert!(cache.get("0").is_none());
        assert_eq!(*cache.get(&MAX_ENTRIES.to_string()).unwrap(), MAX_ENTRIES);
    }

    #[test]
    fn invalidation_rejects_stale_results_and_preserves_existing_arcs() {
        let cache = PlanCache::new(10);
        let old = cache.epoch();
        assert!(cache.insert_if_epoch(old, "key".into(), Arc::new(42), 1));
        let existing = cache.get("key").unwrap();
        let current = cache.invalidate();
        assert_ne!(old, current);
        assert!(cache.get("key").is_none());
        assert_eq!(*existing, 42);
        assert!(!cache.insert_if_epoch(old, "stale".into(), Arc::new(3), 1));
        assert!(cache.insert_if_epoch(current, "fresh".into(), Arc::new(4), 1));
        cache.clear();
        assert!(!cache.insert_if_epoch(current, "stale".into(), Arc::new(5), 1));
    }

    #[test]
    fn oversize_is_not_retained_or_allowed_to_replace_an_entry() {
        let cache = PlanCache::new(5);
        assert!(cache.insert("key".into(), Arc::new(1), 5));
        assert!(!cache.insert("key".into(), Arc::new(2), 6));
        assert!(!cache.insert("other".into(), Arc::new(3), 6));
        assert_eq!(*cache.get("key").unwrap(), 1);
        assert!(cache.get("other").is_none());
    }
}
