use std::{
    borrow::Borrow,
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hash, Hasher},
    sync::Arc,
};

use crate::sparse_array::SparseArray;

const BITS: u32 = 6;
const MASK: u64 = (1u64 << BITS) - 1;

fn compute_hash<K: Hash + ?Sized, S: BuildHasher>(build_hasher: &S, key: &K) -> u64 {
    let mut h = build_hasher.build_hasher();
    key.hash(&mut h);
    h.finish()
}

// `level * BITS` is always <= 60 for valid levels (0..=10), so the shift is in-bounds.
fn bit_index(hash: u64, level: u32) -> usize {
    debug_assert!(level <= 10);
    ((hash >> (level * BITS)) & MASK) as usize
}

#[derive(Clone)]
enum Node<K, V> {
    Leaf { hash: u64, key: K, value: V },
    Interior(SparseArray<Arc<Node<K, V>>>),
    Collision { hash: u64, entries: Vec<(K, V)> },
}

pub struct Hamt<K, V, S = RandomState> {
    root: Option<Arc<Node<K, V>>>,
    len: usize,
    hash_builder: S,
}

impl<K, V> Default for Hamt<K, V, RandomState> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, S: Clone> Clone for Hamt<K, V, S> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
            hash_builder: self.hash_builder.clone(),
        }
    }
}

impl<K, V> Hamt<K, V, RandomState> {
    pub fn new() -> Self {
        Self {
            root: None,
            len: 0,
            hash_builder: RandomState::new(),
        }
    }
}

impl<K, V, S> Hamt<K, V, S> {
    pub fn with_hasher(hash_builder: S) -> Self {
        Self {
            root: None,
            len: 0,
            hash_builder,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Hamt<K, V, S> {
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = compute_hash(&self.hash_builder, key);
        let mut current = self.root.as_deref()?;
        let mut level = 0u32;
        loop {
            match current {
                Node::Leaf {
                    hash: h,
                    key: k,
                    value: v,
                } => {
                    return if *h == hash && k.borrow() == key {
                        Some(v)
                    } else {
                        None
                    };
                }
                Node::Interior(arr) => {
                    current = arr.get(bit_index(hash, level))?;
                    level += 1;
                }
                Node::Collision { hash: h, entries } => {
                    return if *h == hash {
                        entries
                            .iter()
                            .find(|(k, _)| k.borrow() == key)
                            .map(|(_, v)| v)
                    } else {
                        None
                    };
                }
            }
        }
    }

    pub fn insert(&mut self, key: K, value: V)
    where
        K: Clone,
        V: Clone,
    {
        let hash = compute_hash(&self.hash_builder, &key);
        let is_new = match &mut self.root {
            None => {
                self.root = Some(Arc::new(Node::Leaf { hash, key, value }));
                true
            }
            Some(arc) => insert_node(arc, hash, key, value, 0),
        };
        if is_new {
            self.len += 1;
        }
    }

    pub fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Clone + Borrow<Q>,
        V: Clone,
        Q: Hash + Eq + ?Sized,
    {
        let Some(arc) = self.root.take() else {
            return false;
        };
        let hash = compute_hash(&self.hash_builder, key);
        let (new_root, was_removed) = remove_node(arc, hash, key, 0);
        self.root = new_root;
        if was_removed {
            self.len -= 1;
        }
        was_removed
    }
}

// Returns true if the key was newly inserted (false = value was replaced).
fn insert_node<K, V>(arc: &mut Arc<Node<K, V>>, hash: u64, key: K, value: V, level: u32) -> bool
where
    K: Clone + Eq,
    V: Clone,
{
    match arc.as_ref() {
        Node::Leaf {
            hash: h,
            key: k,
            value: v,
        } => {
            if *h == hash {
                if k == &key {
                    *Arc::make_mut(arc) = Node::Leaf { hash, key, value };
                    false
                } else {
                    let entries = vec![(k.clone(), v.clone()), (key, value)];
                    *Arc::make_mut(arc) = Node::Collision { hash, entries };
                    true
                }
            } else {
                let existing_hash = *h;
                let existing_bit = bit_index(existing_hash, level);
                let new_bit = bit_index(hash, level);
                // Borrow of arc via h, k, v ends here (NLL).
                if existing_bit == new_bit {
                    // Both keys land on the same child slot; push them down one level.
                    let mut subtrie = arc.clone();
                    let is_new = insert_node(&mut subtrie, hash, key, value, level + 1);
                    *arc = Arc::new(Node::Interior(
                        SparseArray::default().with_insert(existing_bit, subtrie),
                    ));
                    is_new
                } else {
                    let existing = arc.clone();
                    let new_leaf = Arc::new(Node::Leaf { hash, key, value });
                    *arc = Arc::new(Node::Interior(
                        SparseArray::default()
                            .with_insert(existing_bit, existing)
                            .with_insert(new_bit, new_leaf),
                    ));
                    true
                }
            }
        }
        Node::Interior(arr) => {
            let bit = bit_index(hash, level);
            let occupied = arr.bitmap().has(bit);
            // Borrow of arc via arr ends here (NLL).
            let Node::Interior(arr) = Arc::make_mut(arc) else {
                unreachable!()
            };
            if occupied {
                let Ok(idx) = arr.bitmap().rank(bit) else {
                    unreachable!()
                };
                insert_node(&mut arr.entries_mut()[idx], hash, key, value, level + 1)
            } else {
                let new_leaf = Arc::new(Node::Leaf { hash, key, value });
                let new_arr = arr.with_insert(bit, new_leaf);
                *arr = new_arr;
                true
            }
        }
        Node::Collision { hash: h, entries } => {
            debug_assert_eq!(*h, hash);
            let pos = entries.iter().position(|(k, _)| k == &key);
            // Borrow of arc via h, entries ends here (NLL).
            let Node::Collision { entries, .. } = Arc::make_mut(arc) else {
                unreachable!()
            };
            if let Some(pos) = pos {
                entries[pos].1 = value;
                false
            } else {
                entries.push((key, value));
                true
            }
        }
    }
}

// Returns the updated node (None = deleted) and whether a key was removed.
fn remove_node<K, V, Q>(
    mut arc: Arc<Node<K, V>>,
    hash: u64,
    key: &Q,
    level: u32,
) -> (Option<Arc<Node<K, V>>>, bool)
where
    K: Clone + Borrow<Q>,
    V: Clone,
    Q: Eq + ?Sized,
{
    match arc.as_ref() {
        Node::Leaf {
            hash: h, key: k, ..
        } => {
            if *h == hash && k.borrow() == key {
                (None, true)
            } else {
                (Some(arc), false)
            }
        }
        Node::Interior(arr) => {
            let bit = bit_index(hash, level);
            let Ok(idx) = arr.bitmap().rank(bit) else {
                return (Some(arc), false);
            };
            let child = arr.entries()[idx].clone();
            let (new_child_opt, was_removed) = remove_node(child, hash, key, level + 1);
            if !was_removed {
                return (Some(arc), false);
            }
            let new_node = match new_child_opt {
                Some(new_child) => {
                    let new_arr = arr.with_replaced(bit, new_child);
                    // Borrow of arr ends here (NLL).
                    let Node::Interior(arr) = Arc::make_mut(&mut arc) else {
                        unreachable!()
                    };
                    *arr = new_arr;
                    Some(arc)
                }
                None => {
                    let new_arr = arr.with_remove(bit);
                    // Borrow of arr ends here (NLL).
                    if new_arr.bitmap().is_empty() {
                        return (None, true);
                    }
                    // Compress: a single remaining leaf child can be inlined.
                    if new_arr.bitmap().len() == 1 {
                        let only = &new_arr.entries()[0];
                        if matches!(only.as_ref(), Node::Leaf { .. }) {
                            return (Some(only.clone()), true);
                        }
                    }
                    let Node::Interior(arr) = Arc::make_mut(&mut arc) else {
                        unreachable!()
                    };
                    *arr = new_arr;
                    Some(arc)
                }
            };
            (new_node, true)
        }
        Node::Collision { hash: h, entries } => {
            if *h != hash {
                return (Some(arc), false);
            }
            let pos = entries.iter().position(|(k, _)| k.borrow() == key);
            let h = *h;
            let len = entries.len();
            // Borrow of arc via h, entries ends here (NLL).
            let Some(pos) = pos else {
                return (Some(arc), false);
            };
            match len {
                1 => (None, true),
                2 => {
                    // Collapse collision node to a single leaf.
                    let remaining = 1 - pos;
                    let Node::Collision { entries, .. } = arc.as_ref() else {
                        unreachable!()
                    };
                    let (k, v) = (entries[remaining].0.clone(), entries[remaining].1.clone());
                    // Borrow ends here (NLL).
                    *Arc::make_mut(&mut arc) = Node::Leaf {
                        hash: h,
                        key: k,
                        value: v,
                    };
                    (Some(arc), true)
                }
                _ => {
                    let Node::Collision { entries, .. } = Arc::make_mut(&mut arc) else {
                        unreachable!()
                    };
                    entries.swap_remove(pos);
                    (Some(arc), true)
                }
            }
        }
    }
}

pub struct Iter<'a, K, V> {
    stack: Vec<&'a Node<K, V>>,
    collision: Option<std::slice::Iter<'a, (K, V)>>,
    remaining: usize,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ref mut it) = self.collision {
                if let Some((k, v)) = it.next() {
                    self.remaining -= 1;
                    return Some((k, v));
                }
                self.collision = None;
            }

            match self.stack.pop()? {
                Node::Leaf { key, value, .. } => {
                    self.remaining -= 1;
                    return Some((key, value));
                }
                Node::Interior(arr) => {
                    for child in arr.entries().iter().rev() {
                        self.stack.push(child.as_ref());
                    }
                }
                Node::Collision { entries, .. } => {
                    self.collision = Some(entries.iter());
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}

impl<'a, K, V, S> IntoIterator for &'a Hamt<K, V, S> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K, V> FromIterator<(K, V)> for Hamt<K, V>
where
    K: Hash + Eq + Clone,
    V: Clone,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut h = Hamt::new();
        for (k, v) in iter {
            h.insert(k, v);
        }
        h
    }
}

impl<K, V, S> Hamt<K, V, S> {
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            stack: self.root.as_deref().into_iter().collect(),
            collision: None,
            remaining: self.len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let h: Hamt<&str, i32> = Hamt::new();
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
        assert_eq!(h.get("x"), None);
    }

    #[test]
    fn insert_and_get() {
        let mut h = Hamt::new();
        h.insert("a", 1);
        h.insert("b", 2);
        h.insert("c", 3);
        assert_eq!(h.len(), 3);
        assert_eq!(h.get("a"), Some(&1));
        assert_eq!(h.get("b"), Some(&2));
        assert_eq!(h.get("c"), Some(&3));
        assert_eq!(h.get("d"), None);
    }

    #[test]
    fn insert_replaces_value() {
        let mut h = Hamt::new();
        h.insert("a", 1);
        h.insert("a", 2);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("a"), Some(&2));
    }

    #[test]
    fn remove() {
        let mut h = Hamt::new();
        h.insert("a", 1);
        h.insert("b", 2);
        h.insert("c", 3);
        let mut h2 = h.clone();
        assert!(h2.remove("b"));
        assert_eq!(h2.len(), 2);
        assert_eq!(h2.get("a"), Some(&1));
        assert_eq!(h2.get("b"), None);
        assert_eq!(h2.get("c"), Some(&3));
        // Original unchanged.
        assert_eq!(h.get("b"), Some(&2));
    }

    #[test]
    fn remove_missing_key() {
        let mut h = Hamt::new();
        h.insert("a", 1);
        assert!(!h.remove("z"));
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("a"), Some(&1));
    }

    #[test]
    fn persistence() {
        let mut h1 = Hamt::new();
        h1.insert("a", 1);
        let mut h2 = h1.clone();
        h2.insert("b", 2);
        assert_eq!(h1.len(), 1);
        assert_eq!(h1.get("b"), None);
        assert_eq!(h2.len(), 2);
        assert_eq!(h2.get("a"), Some(&1));
        assert_eq!(h2.get("b"), Some(&2));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn many_inserts_and_gets() {
        let mut h = Hamt::new();
        for i in 0u32..1000 {
            h.insert(i, i * 2);
        }
        assert_eq!(h.len(), 1000);
        for i in 0u32..1000 {
            assert_eq!(h.get(&i), Some(&(i * 2)));
        }
    }

    #[test]
    fn iter_empty() {
        let h: Hamt<&str, i32> = Hamt::new();
        assert_eq!(h.iter().count(), 0);
        assert_eq!(h.iter().len(), 0);
    }

    #[test]
    fn iter_yields_all_pairs() {
        let mut h = Hamt::new();
        h.insert("a", 1);
        h.insert("b", 2);
        h.insert("c", 3);
        let mut pairs: Vec<_> = h.iter().map(|(&k, &v)| (k, v)).collect();
        pairs.sort();
        assert_eq!(pairs, vec![("a", 1), ("b", 2), ("c", 3)]);
    }

    #[test]
    fn iter_size_hint_is_exact() {
        let mut h = Hamt::new();
        h.insert(1u32, 'a');
        h.insert(2, 'b');
        let mut it = h.iter();
        assert_eq!(it.len(), 2);
        it.next();
        assert_eq!(it.len(), 1);
        it.next();
        assert_eq!(it.len(), 0);
    }

    #[test]
    fn into_iter() {
        let mut h = Hamt::new();
        h.insert("x", 10);
        h.insert("y", 20);
        let mut pairs: Vec<_> = (&h).into_iter().map(|(&k, &v)| (k, v)).collect();
        pairs.sort();
        assert_eq!(pairs, vec![("x", 10), ("y", 20)]);
    }

    #[test]
    fn from_iterator() {
        let h: Hamt<_, _> = [("a", 1), ("b", 2), ("c", 3)].into_iter().collect();
        assert_eq!(h.len(), 3);
        assert_eq!(h.get("b"), Some(&2));
    }

    #[test]
    fn with_hasher() {
        use std::collections::hash_map::RandomState;
        let mut h: Hamt<&str, i32, _> = Hamt::with_hasher(RandomState::new());
        h.insert("a", 1);
        assert_eq!(h.get("a"), Some(&1));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn many_removes() {
        let mut h = Hamt::new();
        for i in 0u32..100 {
            h.insert(i, i);
        }
        for i in 0u32..100 {
            assert!(h.remove(&i));
        }
        assert!(h.is_empty());
    }
}
