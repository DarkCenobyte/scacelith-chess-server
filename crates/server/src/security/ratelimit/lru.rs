//! A bounded map with an access order: [`LruMap::get`] and [`LruMap::insert`] make an entry the
//! most recent, [`LruMap::peek`] does not, and an insertion at capacity evicts the least recent
//! entry. Every operation is O(1) (a hash index over a slab-allocated doubly linked list).

use std::collections::HashMap;
use std::fmt;

const NIL: usize = usize::MAX;

struct Node<V> {
    key: String,
    value: V,
    /// Towards the oldest entry.
    prev: usize,
    /// Towards the newest entry.
    next: usize,
}

/// A map of at most `max` entries; the least recently used one is evicted first.
pub struct LruMap<V> {
    max: usize,
    index: HashMap<String, usize>,
    nodes: Vec<Option<Node<V>>>,
    free: Vec<usize>,
    oldest: usize,
    newest: usize,
}

impl<V> fmt::Debug for LruMap<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LruMap").field("len", &self.len()).field("max", &self.max).finish()
    }
}

impl<V> LruMap<V> {
    /// An empty map of at most `max` entries (at least 1).
    pub fn new(max: usize) -> LruMap<V> {
        LruMap {
            max: max.max(1),
            index: HashMap::new(),
            nodes: Vec::new(),
            free: Vec::new(),
            oldest: NIL,
            newest: NIL,
        }
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True when the map holds no entry.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The capacity.
    pub fn capacity(&self) -> usize {
        self.max
    }

    fn node(&self, i: usize) -> &Node<V> {
        self.nodes[i].as_ref().expect("linked slots are occupied")
    }

    fn node_mut(&mut self, i: usize) -> &mut Node<V> {
        self.nodes[i].as_mut().expect("linked slots are occupied")
    }

    fn unlink(&mut self, i: usize) {
        let (prev, next) = {
            let n = self.node(i);
            (n.prev, n.next)
        };
        if prev == NIL {
            self.oldest = next;
        } else {
            self.node_mut(prev).next = next;
        }
        if next == NIL {
            self.newest = prev;
        } else {
            self.node_mut(next).prev = prev;
        }
    }

    fn link_newest(&mut self, i: usize) {
        let newest = self.newest;
        {
            let n = self.node_mut(i);
            n.prev = newest;
            n.next = NIL;
        }
        if newest == NIL {
            self.oldest = i;
        } else {
            self.node_mut(newest).next = i;
        }
        self.newest = i;
    }

    fn touch(&mut self, i: usize) {
        if self.newest != i {
            self.unlink(i);
            self.link_newest(i);
        }
    }

    fn take_slot(&mut self, i: usize) -> (String, V) {
        self.unlink(i);
        let n = self.nodes[i].take().expect("linked slots are occupied");
        self.free.push(i);
        self.index.remove(&n.key);
        (n.key, n.value)
    }

    /// The value of `key`, made the most recent entry.
    pub fn get(&mut self, key: &str) -> Option<&mut V> {
        let i = *self.index.get(key)?;
        self.touch(i);
        Some(&mut self.node_mut(i).value)
    }

    /// The value of `key`, without changing the order.
    pub fn peek(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|&i| &self.node(i).value)
    }

    /// The value of `key` to change, without changing the order.
    pub fn peek_mut(&mut self, key: &str) -> Option<&mut V> {
        let i = *self.index.get(key)?;
        Some(&mut self.node_mut(i).value)
    }

    /// True when `key` has an entry.
    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    /// Sets the value of `key` and makes it the most recent entry. A new key at capacity evicts
    /// the least recent entry first, which is returned.
    pub fn insert(&mut self, key: &str, value: V) -> Option<(String, V)> {
        if let Some(&i) = self.index.get(key) {
            self.node_mut(i).value = value;
            self.touch(i);
            return None;
        }
        let evicted = if self.len() >= self.max { self.pop_oldest() } else { None };
        let node = Node { key: key.to_string(), value, prev: NIL, next: NIL };
        let i = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = Some(node);
                i
            }
            None => {
                self.nodes.push(Some(node));
                self.nodes.len() - 1
            }
        };
        self.index.insert(key.to_string(), i);
        self.link_newest(i);
        evicted
    }

    /// Removes `key`; returns its value.
    pub fn remove(&mut self, key: &str) -> Option<V> {
        let i = *self.index.get(key)?;
        Some(self.take_slot(i).1)
    }

    /// Removes and returns the least recent entry.
    pub fn pop_oldest(&mut self) -> Option<(String, V)> {
        (self.oldest != NIL).then(|| self.take_slot(self.oldest))
    }

    /// The entries from the least recent to the most recent.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        let mut i = self.oldest;
        std::iter::from_fn(move || {
            if i == NIL {
                return None;
            }
            let n = self.node(i);
            i = n.next;
            Some((n.key.as_str(), &n.value))
        })
    }

    /// Removes every entry for which `pred` is true, among the `scan` least recent ones; returns
    /// how many were removed.
    pub fn remove_oldest_where(&mut self, scan: usize, mut pred: impl FnMut(&V) -> bool) -> usize {
        let mut i = self.oldest;
        let mut removed = 0;
        for _ in 0..scan {
            if i == NIL {
                break;
            }
            let next = self.node(i).next;
            if pred(&self.node(i).value) {
                self.take_slot(i);
                removed += 1;
            }
            i = next;
        }
        removed
    }

    /// Removes every entry for which `pred` is true; returns how many were removed.
    pub fn remove_where(&mut self, pred: impl FnMut(&V) -> bool) -> usize {
        self.remove_oldest_where(usize::MAX, pred)
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.index.clear();
        self.nodes.clear();
        self.free.clear();
        self.oldest = NIL;
        self.newest = NIL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys<V>(m: &LruMap<V>) -> Vec<String> {
        m.iter().map(|(k, _)| k.to_string()).collect()
    }

    #[test]
    fn evicts_the_least_recently_used_entry() {
        let mut m = LruMap::new(2);
        m.insert("a", 1);
        m.insert("b", 2);
        m.get("a");
        assert_eq!(m.insert("c", 3), Some(("b".to_string(), 2)));
        assert_eq!(keys(&m), ["a", "c"]);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn peek_keeps_the_order_and_insert_refreshes() {
        let mut m = LruMap::new(3);
        for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
            m.insert(k, v);
        }
        assert_eq!(m.peek("a"), Some(&1));
        *m.peek_mut("a").unwrap() = 10;
        assert_eq!(keys(&m), ["a", "b", "c"]);
        m.insert("a", 11);
        assert_eq!(keys(&m), ["b", "c", "a"]);
        assert_eq!(m.remove("c"), Some(3));
        assert_eq!(keys(&m), ["b", "a"]);
        assert_eq!(m.pop_oldest(), Some(("b".to_string(), 2)));
        assert_eq!(keys(&m), ["a"]);
        assert!(m.contains("a") && !m.contains("b"));
        m.clear();
        assert!(m.is_empty() && m.pop_oldest().is_none());
    }

    #[test]
    fn slots_are_reused_and_bounded() {
        let mut m = LruMap::new(100);
        for i in 0..1000 {
            m.insert(&format!("k{i}"), i);
        }
        assert_eq!(m.len(), 100);
        assert!(m.nodes.len() <= 101);
        assert_eq!(keys(&m).first().map(String::as_str), Some("k900"));
        assert_eq!(LruMap::<u8>::new(0).capacity(), 1);
    }

    #[test]
    fn removal_by_predicate() {
        let mut m = LruMap::new(10);
        for i in 0..10 {
            m.insert(&i.to_string(), i);
        }
        assert_eq!(m.remove_oldest_where(4, |v| v % 2 == 0), 2);
        assert_eq!(keys(&m), ["1", "3", "4", "5", "6", "7", "8", "9"]);
        assert_eq!(m.remove_where(|v| *v > 6), 3);
        assert_eq!(keys(&m), ["1", "3", "4", "5", "6"]);
    }
}
