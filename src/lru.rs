use std::collections::HashMap;

/// A cached value that knows what it costs to keep resident.
pub trait ByteSized {
    fn byte_size(&self) -> usize;
}

struct Entry<V> {
    value: V,
    bytes: usize,
    last_used: u64,
}

/// Byte-capped LRU keyed by URI, shared by the loaders that would otherwise
/// grow without bound for the life of the session. Recency is a monotonic
/// counter rather than a clock reading, so ordering never depends on how fast
/// frames arrive or on the system clock moving backwards.
pub struct ByteLru<V> {
    map: HashMap<String, Entry<V>>,
    total_bytes: usize,
    seq: u64,
    cap: usize,
}

impl<V: Clone + ByteSized> ByteLru<V> {
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            total_bytes: 0,
            seq: 0,
            cap,
        }
    }

    pub fn get(&mut self, uri: &str) -> Option<V> {
        self.seq += 1;
        let now = self.seq;
        let entry = self.map.get_mut(uri)?;
        entry.last_used = now;
        Some(entry.value.clone())
    }

    pub fn put(&mut self, uri: String, value: V) {
        let bytes = value.byte_size();
        if let Some(old) = self.map.remove(&uri) {
            self.total_bytes -= old.bytes;
        }
        // Evict least-recently-used until the newcomer fits (always keep the
        // newcomer, even if it alone exceeds the cap).
        while self.total_bytes + bytes > self.cap {
            let lru = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            match lru {
                Some(k) => {
                    if let Some(removed) = self.map.remove(&k) {
                        self.total_bytes -= removed.bytes;
                    }
                }
                None => break,
            }
        }
        self.seq += 1;
        let now = self.seq;
        self.total_bytes += bytes;
        self.map.insert(
            uri,
            Entry {
                value,
                bytes,
                last_used: now,
            },
        );
    }

    pub fn forget(&mut self, uri: &str) {
        if let Some(removed) = self.map.remove(uri) {
            self.total_bytes -= removed.bytes;
        }
    }

    pub fn forget_all(&mut self) {
        self.map.clear();
        self.total_bytes = 0;
    }

    pub fn byte_size(&self) -> usize {
        self.total_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ByteSized for Vec<u8> {
        fn byte_size(&self) -> usize {
            self.len()
        }
    }

    fn val(bytes: usize) -> Vec<u8> {
        vec![0u8; bytes]
    }

    #[test]
    fn evicts_least_recently_used_past_cap() {
        let mut c: ByteLru<Vec<u8>> = ByteLru::new(100); // fits two 40-byte entries
        c.put("a".into(), val(40));
        c.put("b".into(), val(40));
        c.put("c".into(), val(40)); // 120 > cap -> evict LRU (a)
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_some());
        assert!(c.byte_size() <= 100);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c: ByteLru<Vec<u8>> = ByteLru::new(100);
        c.put("a".into(), val(40));
        c.put("b".into(), val(40));
        assert!(c.get("a").is_some()); // bump a; b becomes LRU
        c.put("c".into(), val(40)); // evict LRU (b)
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_none());
        assert!(c.get("c").is_some());
    }

    #[test]
    fn forget_and_forget_all_drop_bytes() {
        let mut c: ByteLru<Vec<u8>> = ByteLru::new(1000);
        c.put("a".into(), val(40));
        c.put("b".into(), val(40));
        c.forget("a");
        assert!(c.get("a").is_none());
        assert_eq!(c.byte_size(), 40);
        c.forget_all();
        assert!(c.get("b").is_none());
        assert_eq!(c.byte_size(), 0);
    }

    #[test]
    fn filling_exactly_to_cap_evicts_nothing() {
        let mut c: ByteLru<Vec<u8>> = ByteLru::new(80); // holds exactly two 40-byte entries
        c.put("a".into(), val(40));
        c.put("b".into(), val(40)); // total == cap: must not trigger eviction
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_some());
        assert_eq!(c.byte_size(), 80);
    }

    #[test]
    fn replacing_a_key_does_not_double_count() {
        let mut c: ByteLru<Vec<u8>> = ByteLru::new(1000);
        c.put("a".into(), val(40));
        c.put("a".into(), val(10));
        assert_eq!(c.byte_size(), 10);
        assert_eq!(c.get("a").map(|v| v.len()), Some(10));
    }

    #[test]
    fn oversized_newcomer_is_kept_alone() {
        let mut c: ByteLru<Vec<u8>> = ByteLru::new(50);
        c.put("a".into(), val(40));
        // Bigger than the cap by itself: everything else goes, it stays.
        c.put("big".into(), val(500));
        assert!(c.get("a").is_none());
        assert!(c.get("big").is_some());
    }
}
