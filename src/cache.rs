//! Small in-memory cache for values that are expensive to fetch, like lists from ClickUp.
//! Each entry remembers when it was stored; readers decide how old is too old.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Entry = (Instant, Arc<dyn Any + Send + Sync>);

#[derive(Default)]
pub struct TtlCache {
    map: Mutex<HashMap<String, Entry>>,
}

impl TtlCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The value under `key` if it is a `T` stored less than `max_age` ago.
    pub fn get<T: Any + Send + Sync>(&self, key: &str, max_age: Duration) -> Option<Arc<T>> {
        let map = self.lock();
        let (at, value) = map.get(key)?;
        if at.elapsed() >= max_age {
            return None;
        }
        value.clone().downcast().ok()
    }

    pub fn insert<T: Any + Send + Sync>(&self, key: String, value: T) -> Arc<T> {
        let value = Arc::new(value);
        self.lock().insert(key, (Instant::now(), value.clone()));
        value
    }

    /// Drops every entry whose key starts with `prefix`.
    pub fn remove_prefix(&self, prefix: &str) {
        self.lock().retain(|k, _| !k.starts_with(prefix));
    }

    /// Drops entries older than `max_age`.
    pub fn prune(&self, max_age: Duration) {
        self.lock().retain(|_, (at, _)| at.elapsed() <= max_age);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_get_age_and_prefix_removal() {
        let cache = TtlCache::default();
        let minute = Duration::from_secs(60);
        cache.insert("a:1".into(), 5_u32);
        cache.insert("a:2".into(), "x".to_owned());
        cache.insert("b:1".into(), 7_u32);

        assert_eq!(*cache.get::<u32>("a:1", minute).unwrap(), 5);
        assert!(cache.get::<String>("a:1", minute).is_none(), "wrong type");
        assert!(cache.get::<u32>("a:1", Duration::ZERO).is_none(), "too old");
        assert!(cache.get::<u32>("missing", minute).is_none());

        cache.remove_prefix("a:");
        assert!(cache.get::<u32>("a:1", minute).is_none());
        assert!(cache.get::<String>("a:2", minute).is_none());
        assert_eq!(*cache.get::<u32>("b:1", minute).unwrap(), 7);

        cache.prune(Duration::ZERO);
        assert!(cache.get::<u32>("b:1", minute).is_none());
    }
}
