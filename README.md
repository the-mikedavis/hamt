# hamt

A reference implementation of a **Hash Array Mapped Trie** (HAMT) in Rust.

This crate is not intended for actual use. It exists to explain how HAMTs work through readable code. If you need a persistent hash map for real use, look elsewhere. The [`im` crate](https://crates.io/crates/im) is a good starting point.

## Table of Contents

1. [Why persistent data structures?](#why-persistent-data-structures)
2. [Tries](#tries)
3. [Array Mapped Tries and sparse arrays](#array-mapped-tries-and-sparse-arrays)
4. [The bitmap and rank](#the-bitmap-and-rank)
5. [Structural sharing](#structural-sharing)
6. [In the wild](#in-the-wild)
7. [This implementation](#this-implementation)
8. [Usage](#usage)
9. [Further reading](#further-reading)

## Why persistent data structures?

A *persistent* data structure preserves all previous versions of itself when modified. This is different from the ordinary, *ephemeral* data structures most programmers reach for: when you `insert` into a Rust `HashMap`, the old state is gone.

The naive solution to keeping old versions is cloning: copy the whole map before mutating. For small maps that is fine, but the cost is O(n) per operation (proportional to the number of entries, not the size of the change). For large maps modified frequently, this becomes a bottleneck.

Persistent data structures solve this by sharing structure between versions. A modification produces a new version that reuses most of the old version's memory. Only the nodes on the path to the changed value need to be newly allocated; everything else is shared by reference.

The result is O(log n) time per operation, worse than the O(1) of a hash table for a single version but far better than O(n) if you need to keep history. And `clone` is O(1): it just increments a reference count.

### When do you want this?

* Concurrency: shared ownership of an immutable value needs no lock. A writer produces a new version and publishes it atomically; concurrent readers hold references to old versions and see a consistent snapshot for as long as they hold it. No mutex, no read-write lock, no reader-writer problem.
* Functional programming style: persistent maps compose naturally with pure functions. You can pass a map into a function without worrying that the function will mutate it, and the function can return a modified version without affecting the caller.
* Undo and redo: storing a history of map states is cheap. Each version shares structure with its neighbors. Rolling back is just a pointer swap.
* Software transactional memory: persistent data structures fit naturally into STM systems, where a transaction works on a private version and commits by swapping it in atomically.

## Tries

A *trie* (from "re**trie**val", sometimes pronounced "try") is a tree whose structure encodes its keys. In a classical string trie, each edge represents one character and every path from root to leaf spells out a key. Tries have the useful property that all keys sharing a prefix share the corresponding nodes.

For a hash map, we do not want to store the full key as the trie path, since keys can be arbitrary and long. Instead we hash the key and use the hash value as the path. A 64-bit hash contains plenty of bits to route a key through a deep-enough trie.

How deep? We consume the hash a few bits at a time. Using 6 bits per level gives 64 possible children per node (2^6). A 64-bit hash supports at most 11 levels before we exhaust it (ceil(64 / 6)). In practice the trie is much shallower: a map with millions of entries rarely needs more than four or five levels.

At the leaves we store the actual key-value pair. Two different keys can produce the same hash (a "hash collision"), but this is rare and the implementation handles it with a dedicated collision node that stores a small linear list.

## Array Mapped Tries and sparse arrays

A straightforward trie node at each level would hold 64 child pointers, one per possible 6-bit value. Most would be null. For a small map, almost every interior node is nearly empty, which wastes memory proportional to the branching factor rather than the number of entries.

Phil Bagwell's _Array Mapped Trie_ (2001) compresses this. Instead of a full 64-element array, each node stores only its occupied children in a compact array, plus a 64-bit bitmap that records which of the 64 slots are present.

```
Bitmap: 0b...00001010   (bits 1 and 3 are set, 2 children total)
Entries: [ child_1, child_3 ]
```

A node with `k` occupied children uses space for exactly `k` child pointers, regardless of the 64-way branching factor. Sparse nodes (the common case) consume almost no memory.

This compact array is the `SparseArray<T>` type in this codebase. It is a single heap allocation: the bitmap lives at the start, immediately followed by the `k` entries. The type is pointer-sized (one word, not two) by representing the trailing entries as a zero-length placeholder `[T; 0]` and managing the rest of the allocation explicitly.

## The bitmap and rank

Given a bitmap, looking up a logical slot `b` takes two steps:

1. **Check presence**: is bit `b` set in the bitmap?
2. **Find the physical index**: how many occupied slots come before slot `b`?

Step 2 is the "rank" of `b` in the bitmap: the number of set bits in positions 0 through b-1. This is exactly one `POPCNT` instruction on any modern CPU, a constant-time operation.

```
Bitmap: 0b00001010  (bits 1 and 3 are occupied)
rank(3) = popcount(0b00001010 & ((1 << 3) - 1))
        = popcount(0b00000010)
        = 1   (so child_3 is at entries[1])
```

Insertion into the compact array creates a new allocation with one extra slot, shifting entries after the insertion point. Deletion creates one with one fewer slot. Both are O(k) in the number of occupied children of the node, and since that is bounded by 64, they are effectively O(1).

## Structural sharing

When a key-value pair is inserted, the path from the root to the new leaf is walked. At each interior node along that path, a new node is allocated that contains the new child pointer but reuses all other children from the old version. All nodes not on that path are shared unchanged between the old and new versions.

```
Before insert("c", 3):

    root --- [a=1, b=2]

After insert("c", 3):

 new_root --- [a=1, b=2, c=3]
                  ^       ^
               shared   new leaf
```

In a deeper tree the same principle applies level by level. Only O(depth) = O(log n) nodes are newly allocated per operation.

Old versions remain live for as long as someone holds a reference to them. `Arc` (atomic reference counting) manages this automatically: nodes are freed only when no version still references them.

The copy-on-write nuance: when two versions share an interior node and one wants to modify it, it must first make a private copy. Rust's standard library provides `Arc::make_mut` for this: if the `Arc` has a single owner it hands back a mutable reference directly (no allocation); if it is shared it clones the inner value first. A freshly built `Hamt` that has never been cloned can be mutated with zero cloning overhead on interior nodes.

## In the wild

HAMTs are not just academic. They are the implementation behind the built-in map types in several production programming languages.

Clojure introduced HAMTs to widespread use in the JVM world. Rich Hickey designed Clojure's `PersistentHashMap` (2007) around Bagwell's structure, adapting it to the JVM's garbage collector (which plays the role that `Arc` plays here). Clojure's design emphasised value semantics (data is immutable by default) and the HAMT made that practical for maps.

Erlang (since OTP 17, 2014) and Elixir use a HAMT for the built-in map type. Erlang's design priorities (lightweight concurrent processes, message passing, no shared mutable state) make persistent data structures a natural fit. Maps are sent between processes as values; there is no concept of a shared mutable reference.

Haskell's `unordered-containers` package provides `HashMap` and `HashSet` backed by a HAMT.

Scala's `scala.collection.immutable.HashMap` is a HAMT.

In all of these languages the HAMT is the answer to the same question: how do you provide a hash map interface (at-worst O(log n) insert, lookup, and delete) while keeping the value semantics and cheap persistence that purely functional or message-passing designs require?

## This implementation

This codebase implements the core HAMT structure and uses a few techniques worth understanding.

### Node types

There are three kinds of nodes:

```rust
enum Node<K, V> {
    Leaf { hash: u64, key: K, value: V },
    Interior(SparseArray<Arc<Node<K, V>>>),
    Collision { hash: u64, entries: Vec<(K, V)> },
}
```

`Leaf` stores a single key-value pair. `Interior` is a sparse array of reference-counted child nodes. `Collision` handles the rare case where two keys produce an identical 64-bit hash; a linear scan over a handful of entries is the right tool for something that happens essentially never.

### `SparseArray<T>`

Rather than using Rust's DST machinery (which forces fat pointers and complicates trait implementations), `SparseArray<T>` stores its data as a manually managed heap allocation:

```
[ Bitmap (8 bytes) | T | T | T | ... ]
```

The Rust type is `NonNull<SparseArrayInner<T>>` where `SparseArrayInner` ends with a `[T; 0]` placeholder. This makes `SparseArray<T>` exactly one pointer wide and allows it to implement `Clone`, `Default`, and `Drop` in the normal way.

### Iterative insertion

Insertion descends through interior nodes level by level. A naive recursive implementation allocates one stack frame per level; an iterative one uses a raw pointer cursor to advance through the tree without recursion. The cursor is safe because exclusive access is guaranteed by the `&mut` borrow at the call site, and every node it visits is kept alive by the trie above it.

### Pluggable hasher

Like `std::collections::HashMap`, the hasher is a type parameter `S: BuildHasher` with a default of `RandomState`:

```rust
pub struct Hamt<K, V, S = RandomState> { ... }
```

This lets callers supply a deterministic hasher for tests or a faster non-cryptographic hasher for performance.

## Usage

The API mirrors a standard hash map.

```rust
use hamt::Hamt;

let mut h = Hamt::new();
h.insert("one", 1);
h.insert("two", 2);
h.insert("three", 3);

assert_eq!(h.get("two"), Some(&2));
assert_eq!(h.get("four"), None);

h.remove("two");
assert_eq!(h.len(), 2);
```

`clone` is O(1): it increments one reference count. Independently mutating the clone does not affect the original and vice versa, since the two versions share the nodes they have in common.

```rust
let mut a = Hamt::new();
a.insert("x", 10);

let mut b = a.clone();  // O(1)
b.insert("y", 20);

assert_eq!(a.get("y"), None);       // a is unchanged
assert_eq!(b.get("x"), Some(&10));  // b shares a's data
```

The map is iterable:

```rust
for (key, value) in &h {
    println!("{key}: {value}");
}
```

## References

- Phil Bagwell, *Ideal Hash Trees* (2001). The original paper describing the Array Mapped Trie and its application to hash maps.
- Rich Hickey, *Persistent Data Structures and Managed References* (2009). A talk and essay on the philosophy behind Clojure's immutable collections.
- The [`im` crate](https://crates.io/crates/im). A production-ready persistent collections library for Rust, including a HAMT-backed `HashMap` and `HashSet`.
