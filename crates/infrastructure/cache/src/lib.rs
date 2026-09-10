/*!
 * @file TtlLruCache
 * @description Bounded cache with TTL expiry and LRU eviction.
 *
 * Responsibilities:
 * - Serve fresh entries and report misses explicitly.
 * - Expire entries past their deadline on access.
 * - Evict least-recently-used entries when full.
 *
 * This module must not depend on: any other workspace crate. Time
 * enters as caller timestamps, keeping every policy deterministic.
 */

//! Cache as explicit state machine: no background threads, no hidden clocks.

use std::collections::{HashMap, VecDeque};

/// Bounded TTL cache with LRU eviction.
#[derive(Debug, Default)]
pub struct TtlLruCache<V> {
    capacity: usize,
    ttl_secs: u64,
    entries: HashMap<String, (V, u64)>,
    order: VecDeque<String>,
}

impl<V> TtlLruCache<V> {
    /// Create a cache holding at most `capacity` entries for `ttl_secs`.
    pub fn new(capacity: usize, ttl_secs: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl_secs,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Fetch one entry, expiring and bumping recency on the way.
    pub fn get(&mut self, key: &str, now_secs: u64) -> Option<&V> {
        let expired = match self.entries.get(key) {
            Some((_, stored_at)) => now_secs.saturating_sub(*stored_at) > self.ttl_secs,
            None => return None,
        };
        if expired {
            self.entries.remove(key);
            self.order.retain(|k| k != key);
            return None;
        }
        self.order.retain(|k| k != key);
        self.order.push_back(key.to_string());
        self.entries.get(key).map(|(value, _)| value)
    }

    /// Store one entry, evicting the least-recently-used entry when full.
    pub fn put(&mut self, key: impl Into<String>, value: V, now_secs: u64) {
        let key = key.into();
        if self.entries.contains_key(&key) {
            self.order.retain(|k| k != &key);
        } else {
            while self.entries.len() >= self.capacity {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, (value, now_secs));
    }

    /// Live entries after expiring everything past `now_secs`.
    pub fn len(&mut self, now_secs: u64) -> usize {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, (_, stored_at))| now_secs.saturating_sub(*stored_at) > self.ttl_secs)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.entries.remove(&key);
            self.order.retain(|k| k != &key);
        }
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_bump_recency_and_expiry_purges() {
        let mut cache = TtlLruCache::new(2, 10);
        cache.put("a", 1, 0);
        cache.put("b", 2, 0);
        assert_eq!(cache.get("a", 5), Some(&1));
        // `a` is now most-recent; `b` evicts next.
        cache.put("c", 3, 5);
        assert_eq!(cache.get("b", 5), None);
        assert_eq!(cache.get("a", 5), Some(&1));
        // Past the deadline everything misses.
        assert_eq!(cache.get("a", 20), None);
        assert_eq!(cache.len(20), 0);
    }

    #[test]
    fn overwrite_refreshes_without_growing() {
        let mut cache = TtlLruCache::new(2, 100);
        cache.put("a", 1, 0);
        cache.put("a", 2, 10);
        assert_eq!(cache.get("a", 10), Some(&2));
        assert_eq!(cache.len(10), 1);
    }
}
