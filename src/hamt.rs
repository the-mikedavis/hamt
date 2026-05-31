use std::{
    borrow::Borrow,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::sparse_array::SparseArray;

const BITS: u32 = 6;
const MASK: u64 = (1u64 << BITS) - 1;

fn compute_hash<K: Hash + ?Sized>(key: &K) -> u64 {
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

// `level * BITS` is always <= 60 for valid levels (0..=10), so the shift is in-bounds.
fn bit_index(hash: u64, level: u32) -> usize {
    debug_assert!(level <= 10);
    ((hash >> (level * BITS)) & MASK) as usize
}

enum Node<K, V> {
    Leaf { hash: u64, key: K, value: V },
    Interior(SparseArray<Arc<Node<K, V>>>),
    Collision { hash: u64, entries: Vec<(K, V)> },
}

pub struct Hamt<K, V> {
    root: Option<Arc<Node<K, V>>>,
    len: usize,
}

impl<K, V> Default for Hamt<K, V> {
    fn default() -> Self {
        Self { root: None, len: 0 }
    }
}

impl<K, V> Clone for Hamt<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<K: Hash + Eq, V> Hamt<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = compute_hash(key);
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

    pub fn insert(&self, key: K, value: V) -> Self
    where
        K: Clone,
        V: Clone,
    {
        let hash = compute_hash(&key);
        let (new_root, is_new) = insert_node(self.root.clone(), hash, key, value, 0);
        Self {
            root: Some(new_root),
            len: if is_new { self.len + 1 } else { self.len },
        }
    }

    pub fn remove<Q>(&self, key: &Q) -> Self
    where
        K: Clone + Borrow<Q>,
        V: Clone,
        Q: Hash + Eq + ?Sized,
    {
        let Some(root) = self.root.clone() else {
            return Self::default();
        };
        let hash = compute_hash(key);
        let (new_root, was_removed) = remove_node(root, hash, key, 0);
        Self {
            root: new_root,
            len: if was_removed { self.len - 1 } else { self.len },
        }
    }
}

fn insert_node<K, V>(
    node: Option<Arc<Node<K, V>>>,
    hash: u64,
    key: K,
    value: V,
    level: u32,
) -> (Arc<Node<K, V>>, bool)
where
    K: Clone + Eq,
    V: Clone,
{
    let Some(node) = node else {
        return (Arc::new(Node::Leaf { hash, key, value }), true);
    };

    match node.as_ref() {
        Node::Leaf {
            hash: h,
            key: k,
            value: v,
        } => {
            if *h == hash {
                if k == &key {
                    (Arc::new(Node::Leaf { hash, key, value }), false)
                } else {
                    (
                        Arc::new(Node::Collision {
                            hash,
                            entries: vec![(k.clone(), v.clone()), (key, value)],
                        }),
                        true,
                    )
                }
            } else {
                // Different hashes: expand this leaf into an interior node and re-insert both.
                let existing_bit = bit_index(*h, level);
                let new_bit = bit_index(hash, level);
                if existing_bit == new_bit {
                    // Both keys land on the same child slot; push them down one level.
                    let (child, is_new) =
                        insert_node(Some(node.clone()), hash, key, value, level + 1);
                    let arr = SparseArray::default().with_insert(existing_bit, child);
                    (Arc::new(Node::Interior(arr)), is_new)
                } else {
                    let arr = SparseArray::default()
                        .with_insert(existing_bit, node.clone())
                        .with_insert(new_bit, Arc::new(Node::Leaf { hash, key, value }));
                    (Arc::new(Node::Interior(arr)), true)
                }
            }
        }
        Node::Interior(arr) => {
            let bit = bit_index(hash, level);
            let child = arr.get(bit).cloned();
            let (new_child, is_new) = insert_node(child, hash, key, value, level + 1);
            let new_arr = if arr.bitmap().has(bit) {
                arr.with_replaced(bit, new_child)
            } else {
                arr.with_insert(bit, new_child)
            };
            (Arc::new(Node::Interior(new_arr)), is_new)
        }
        Node::Collision { hash: h, entries } => {
            debug_assert_eq!(*h, hash);
            let mut new_entries = entries.clone();
            let is_new = if let Some(pos) = new_entries.iter().position(|(k, _)| k == &key) {
                new_entries[pos].1 = value;
                false
            } else {
                new_entries.push((key, value));
                true
            };
            (
                Arc::new(Node::Collision {
                    hash: *h,
                    entries: new_entries,
                }),
                is_new,
            )
        }
    }
}

fn remove_node<K, V, Q>(
    node: Arc<Node<K, V>>,
    hash: u64,
    key: &Q,
    level: u32,
) -> (Option<Arc<Node<K, V>>>, bool)
where
    K: Clone + Borrow<Q>,
    V: Clone,
    Q: Eq + ?Sized,
{
    match node.as_ref() {
        Node::Leaf {
            hash: h, key: k, ..
        } => {
            if *h == hash && k.borrow() == key {
                (None, true)
            } else {
                (Some(node.clone()), false)
            }
        }
        Node::Interior(arr) => {
            let bit = bit_index(hash, level);
            let Some(child) = arr.get(bit).cloned() else {
                return (Some(node.clone()), false);
            };
            let (new_child_opt, was_removed) = remove_node(child, hash, key, level + 1);
            if !was_removed {
                return (Some(node.clone()), false);
            }
            let new_node = match new_child_opt {
                Some(new_child) => Arc::new(Node::Interior(arr.with_replaced(bit, new_child))),
                None => {
                    let new_arr = arr.with_remove(bit);
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
                    Arc::new(Node::Interior(new_arr))
                }
            };
            (Some(new_node), true)
        }
        Node::Collision { hash: h, entries } => {
            if *h != hash {
                return (Some(node.clone()), false);
            }
            let Some(pos) = entries.iter().position(|(k, _)| k.borrow() == key) else {
                return (Some(node.clone()), false);
            };
            let mut new_entries = entries.clone();
            new_entries.swap_remove(pos);
            let new_node = match new_entries.len() {
                0 => None,
                1 => {
                    let (k, v) = new_entries.remove(0);
                    Some(Arc::new(Node::Leaf {
                        hash: *h,
                        key: k,
                        value: v,
                    }))
                }
                _ => Some(Arc::new(Node::Collision {
                    hash: *h,
                    entries: new_entries,
                })),
            };
            (new_node, true)
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
        let h = Hamt::new().insert("a", 1).insert("b", 2).insert("c", 3);
        assert_eq!(h.len(), 3);
        assert_eq!(h.get("a"), Some(&1));
        assert_eq!(h.get("b"), Some(&2));
        assert_eq!(h.get("c"), Some(&3));
        assert_eq!(h.get("d"), None);
    }

    #[test]
    fn insert_replaces_value() {
        let h = Hamt::new().insert("a", 1).insert("a", 2);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("a"), Some(&2));
    }

    #[test]
    fn remove() {
        let h = Hamt::new().insert("a", 1).insert("b", 2).insert("c", 3);
        let h2 = h.remove("b");
        assert_eq!(h2.len(), 2);
        assert_eq!(h2.get("a"), Some(&1));
        assert_eq!(h2.get("b"), None);
        assert_eq!(h2.get("c"), Some(&3));
    }

    #[test]
    fn remove_missing_key() {
        let h = Hamt::new().insert("a", 1);
        let h2 = h.remove("z");
        assert_eq!(h2.len(), 1);
        assert_eq!(h2.get("a"), Some(&1));
    }

    #[test]
    fn persistence() {
        let h1 = Hamt::new().insert("a", 1);
        let h2 = h1.insert("b", 2);
        assert_eq!(h1.len(), 1);
        assert_eq!(h1.get("b"), None);
        assert_eq!(h2.len(), 2);
        assert_eq!(h2.get("a"), Some(&1));
        assert_eq!(h2.get("b"), Some(&2));
    }

    #[test]
    fn many_inserts_and_gets() {
        let mut h = Hamt::new();
        for i in 0u32..1000 {
            h = h.insert(i, i * 2);
        }
        assert_eq!(h.len(), 1000);
        for i in 0u32..1000 {
            assert_eq!(h.get(&i), Some(&(i * 2)));
        }
    }

    #[test]
    fn many_removes() {
        let mut h = Hamt::new();
        for i in 0u32..100 {
            h = h.insert(i, i);
        }
        for i in 0u32..100 {
            h = h.remove(&i);
        }
        assert!(h.is_empty());
    }
}
