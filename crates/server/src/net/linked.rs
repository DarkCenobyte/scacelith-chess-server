//! An insertion-ordered hash map with O(1) removal anywhere and O(1) move to the back: the
//! JavaScript `Map` the limiters of the Node server relied on (iteration in insertion order,
//! `delete` then `set` to refresh an entry). Used for LRU eviction and "oldest first" bounds.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

const NIL: u32 = u32::MAX;

#[derive(Debug)]
struct Slot<K, V> {
    entry: Option<(K, V)>,
    prev: u32,
    next: u32,
}

/// An insertion-ordered map (see the module documentation).
#[derive(Debug)]
pub struct LinkedMap<K, V> {
    index: HashMap<K, u32>,
    slots: Vec<Slot<K, V>>,
    free: Vec<u32>,
    head: u32,
    tail: u32,
}

impl<K: Hash + Eq + Clone, V> Default for LinkedMap<K, V> {
    fn default() -> Self {
        LinkedMap::new()
    }
}

impl<K: Hash + Eq + Clone, V> LinkedMap<K, V> {
    /// An empty map.
    pub fn new() -> LinkedMap<K, V> {
        LinkedMap { index: HashMap::new(), slots: Vec::new(), free: Vec::new(), head: NIL, tail: NIL }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Whether `key` is present.
    pub fn contains_key<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.index.contains_key(key)
    }

    /// The value of `key`, without changing its place.
    pub fn get<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let i = *self.index.get(key)?;
        self.slots[i as usize].entry.as_ref().map(|(_, v)| v)
    }

    /// The value of `key`, mutable, without changing its place.
    pub fn get_mut<Q: ?Sized + Hash + Eq>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let i = *self.index.get(key)?;
        self.slots[i as usize].entry.as_mut().map(|(_, v)| v)
    }

    /// Sets the value of `key`. A new key goes to the back; an existing key keeps its place (as
    /// `Map.prototype.set`). Returns the previous value.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some(&i) = self.index.get(&key) {
            let slot = self.slots[i as usize].entry.as_mut().expect("indexed slots hold an entry");
            return Some(std::mem::replace(&mut slot.1, value));
        }
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = Slot { entry: Some((key.clone(), value)), prev: NIL, next: NIL };
                i
            }
            None => {
                let i = u32::try_from(self.slots.len()).expect("fewer than 2^32 entries");
                self.slots.push(Slot { entry: Some((key.clone(), value)), prev: NIL, next: NIL });
                i
            }
        };
        self.index.insert(key, i);
        self.link_back(i);
        None
    }

    /// Moves `key` to the back (most recent). Returns whether it was present.
    pub fn move_to_back<Q: ?Sized + Hash + Eq>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        let Some(&i) = self.index.get(key) else { return false };
        if self.tail != i {
            self.unlink(i);
            self.link_back(i);
        }
        true
    }

    /// Removes `key` and returns its value.
    pub fn remove<Q: ?Sized + Hash + Eq>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let i = self.index.remove(key)?;
        self.unlink(i);
        let (_, v) = self.slots[i as usize].entry.take().expect("indexed slots hold an entry");
        self.free.push(i);
        Some(v)
    }

    /// The oldest entry.
    pub fn front(&self) -> Option<(&K, &V)> {
        if self.head == NIL {
            return None;
        }
        self.slots[self.head as usize].entry.as_ref().map(|(k, v)| (k, v))
    }

    /// Removes and returns the oldest entry.
    pub fn pop_front(&mut self) -> Option<(K, V)> {
        let key = self.front()?.0.clone();
        let v = self.remove(&key)?;
        Some((key, v))
    }

    /// The entries, oldest first.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { map: self, at: self.head }
    }

    /// The keys, oldest first.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    /// Keeps the entries for which `keep` returns true (visited oldest first), stopping the walk
    /// early when `keep` asks to (returns `None`). Returns the number of entries removed.
    pub fn retain_while(&mut self, mut keep: impl FnMut(&K, &mut V) -> Option<bool>) -> usize {
        let mut at = self.head;
        let mut removed = 0;
        while at != NIL {
            let next = self.slots[at as usize].next;
            let (k, v) = self.slots[at as usize].entry.as_mut().expect("linked slots hold an entry");
            match keep(k, v) {
                None => break,
                Some(true) => {}
                Some(false) => {
                    let key = k.clone();
                    self.remove(&key);
                    removed += 1;
                }
            }
            at = next;
        }
        removed
    }

    /// Keeps the entries for which `keep` returns true. Returns the number removed.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) -> usize {
        self.retain_while(|k, v| Some(keep(k, v)))
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.index.clear();
        self.slots.clear();
        self.free.clear();
        self.head = NIL;
        self.tail = NIL;
    }

    fn link_back(&mut self, i: u32) {
        self.slots[i as usize].prev = self.tail;
        self.slots[i as usize].next = NIL;
        if self.tail == NIL {
            self.head = i;
        } else {
            self.slots[self.tail as usize].next = i;
        }
        self.tail = i;
    }

    fn unlink(&mut self, i: u32) {
        let (prev, next) = (self.slots[i as usize].prev, self.slots[i as usize].next);
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }
    }
}

/// Iterator over a [`LinkedMap`], oldest first.
#[derive(Debug)]
pub struct Iter<'a, K, V> {
    map: &'a LinkedMap<K, V>,
    at: u32,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at == NIL {
            return None;
        }
        let slot = &self.map.slots[self.at as usize];
        self.at = slot.next;
        slot.entry.as_ref().map(|(k, v)| (k, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_and_moves_to_back() {
        let mut m = LinkedMap::new();
        for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
            m.insert(k, v);
        }
        assert_eq!(m.insert("a", 10), Some(1), "an existing key keeps its place");
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(m.move_to_back(&"a"));
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), ["b", "c", "a"]);
        assert_eq!(m.remove(&"c"), Some(3));
        m.insert("d", 4);
        assert_eq!(m.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(), [("b", 2), ("a", 10), ("d", 4)]);
        assert_eq!(m.pop_front(), Some(("b", 2)));
        assert_eq!(m.front(), Some((&"a", &10)));
        assert_eq!(m.len(), 2);
        assert_eq!(m.retain(|_, v| *v > 5), 1);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), ["a"]);
        m.clear();
        assert!(m.is_empty() && m.front().is_none());
    }

    #[test]
    fn retain_while_stops_early() {
        let mut m = LinkedMap::new();
        for i in 0..10 {
            m.insert(i, i);
        }
        let mut seen = 0;
        let removed = m.retain_while(|_, v| {
            seen += 1;
            if seen > 4 { None } else { Some(*v % 2 == 1) }
        });
        assert_eq!(removed, 2);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), [1, 3, 4, 5, 6, 7, 8, 9]);
    }
}
