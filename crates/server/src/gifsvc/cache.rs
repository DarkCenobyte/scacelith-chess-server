//! The cache of rendered GIFs: an LRU bounded in bytes (`GIF_CACHE_MB`), keyed by the job's
//! hash ([`GifJob::cache_key`](super::GifJob::cache_key)).

use bytes::Bytes;
use indexmap::IndexMap;

/// Byte-bounded LRU of rendered GIFs: the least recently used goes first, and no single GIF may
/// take more than a quarter of it.
#[derive(Debug, Default)]
pub struct GifCache {
    max_bytes: usize,
    /// Oldest first.
    map: IndexMap<String, Bytes>,
    bytes: usize,
}

impl GifCache {
    /// A cache of at most `max_bytes` bytes (0 disables it).
    pub fn new(max_bytes: usize) -> GifCache {
        GifCache { max_bytes, map: IndexMap::new(), bytes: 0 }
    }

    /// GIFs cached.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Bytes of the GIFs cached.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The GIF of `key`, which becomes the most recently used.
    pub fn get(&mut self, key: &str) -> Option<Bytes> {
        let index = self.map.get_index_of(key)?;
        let last = self.map.len() - 1;
        self.map.move_index(index, last);
        Some(self.map[last].clone())
    }

    /// Keeps `gif` under `key` (replacing an older one) unless it is larger than a quarter of the
    /// cache; evicts the least recently used beyond the limit. Returns whether it was kept.
    pub fn set(&mut self, key: &str, gif: Bytes) -> bool {
        if self.max_bytes == 0 || gif.len() > self.max_bytes / 4 {
            return false;
        }
        if let Some(old) = self.map.shift_remove(key) {
            self.bytes -= old.len();
        }
        self.bytes += gif.len();
        self.map.insert(key.to_string(), gif);
        while self.bytes > self.max_bytes {
            let (_, old) = self.map.shift_remove_index(0).expect("bytes are cached");
            self.bytes -= old.len();
        }
        true
    }

    /// Empties the cache.
    pub fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<&str> {
        self.map.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn bounded_in_bytes_least_recently_used_out_no_gif_above_a_quarter_zero_disables() {
        let mut c = GifCache::new(1000);
        for k in ["a", "b", "c"] {
            assert!(c.set(k, gif(200)));
        }
        assert_eq!(c.get("a").map(|g| g.len()), Some(200));
        assert!(c.set("d", gif(250)));
        assert!(c.set("e", gif(240)));
        assert_eq!(c.keys(), ["c", "a", "d", "e"]);
        assert_eq!((c.bytes(), c.len()), (890, 4));
        assert!(!c.set("big", gif(251)));
        assert_eq!(c.get("b"), None);
        // Replacing an entry counts its bytes once.
        assert!(c.set("c", gif(10)));
        assert_eq!(c.keys(), ["a", "d", "e", "c"]);
        assert_eq!(c.bytes(), 700);
        c.clear();
        assert!(c.is_empty() && c.bytes() == 0);
        assert!(!GifCache::new(0).set("x", gif(1)));
    }
}
