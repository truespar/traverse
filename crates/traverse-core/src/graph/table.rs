//! Entity tables and id sets laid out by id. Ids come from counters, so nearly every id falls in
//! a dense prefix: a slab indexed by id answers a lookup with one bounds check instead of a hash
//! probe, and iterating a label or a type visits entities in id order, which keeps property
//! gathers and adjacency reads sequential in memory. Ids far outside the prefix (imports with
//! explicit ids, a sparse table over a wide id range) fall back to a hash map so nothing is ever
//! rejected; when the map fills up with ids that would fit a prefix after all (a file loaded in
//! hash order) they migrate into the slab in one pass.

use rustc_hash::{FxHashMap, FxHashSet};

use super::types::{EdgeId, NodeId};

/// A key that is a plain integer id.
pub trait IdKey: Copy + Eq + std::hash::Hash + 'static {
    /// The integer.
    fn raw(self) -> u64;
    /// The key for an integer.
    fn from_raw(raw: u64) -> Self;
}

impl IdKey for NodeId {
    #[inline]
    fn raw(self) -> u64 {
        self.0
    }
    #[inline]
    fn from_raw(raw: u64) -> Self {
        NodeId(raw)
    }
}

impl IdKey for EdgeId {
    #[inline]
    fn raw(self) -> u64 {
        self.0
    }
    #[inline]
    fn from_raw(raw: u64) -> Self {
        EdgeId(raw)
    }
}

/// Slack past the occupied range a prefix may grow for one insert.
const SLACK: usize = 4096;

/// How far a dense prefix may extend: twice the occupied range (the prefix so far, the entities
/// held, or the expected count), plus slack. A sparse table over a wide id range stays in the map
/// rather than becoming a near-empty slab.
fn dense_bound(prefix: usize, len: usize, hint: usize) -> usize {
    prefix
        .max(len)
        .max(hint)
        .saturating_mul(2)
        .saturating_add(SLACK)
}

/// Entities by id: a dense slab for the id prefix, a map for far-out ids.
#[derive(Debug, Clone)]
pub struct IdTable<K: IdKey, V> {
    dense: Vec<Option<V>>,
    far: FxHashMap<K, V>,
    len: usize,
    /// Entities expected, from the constructor.
    hint: usize,
    /// Highest id in the map, one past.
    far_end: usize,
}

impl<K: IdKey, V> Default for IdTable<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: IdKey, V> IdTable<K, V> {
    /// An empty table.
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// An empty table expecting `n` entities with ids in a prefix of about that length.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            dense: Vec::with_capacity(n),
            far: FxHashMap::default(),
            len: 0,
            hint: n,
            far_end: 0,
        }
    }

    #[inline]
    fn bound(&self) -> usize {
        dense_bound(self.dense.len(), self.len, self.hint)
    }

    /// The entity with `key`.
    #[inline]
    pub fn get(&self, key: &K) -> Option<&V> {
        let i = key.raw() as usize;
        if let Some(Some(v)) = self.dense.get(i) {
            return Some(v);
        }
        if self.far.is_empty() {
            None
        } else {
            self.far.get(key)
        }
    }

    /// The entity with `key`, mutable.
    #[inline]
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let i = key.raw() as usize;
        if i < self.dense.len() && self.dense[i].is_some() {
            return self.dense[i].as_mut();
        }
        if self.far.is_empty() {
            None
        } else {
            self.far.get_mut(key)
        }
    }

    /// Is `key` present?
    #[inline]
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Inserts `value` under `key`, returning what was there.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let i = key.raw() as usize;
        if i < self.dense.len() {
            let old = self.dense[i].replace(value);
            if old.is_none() {
                // a far entry for this id predates the prefix reaching it
                let far_old = self.far.remove(&key);
                if far_old.is_none() {
                    self.len += 1;
                    self.maybe_migrate();
                }
                return far_old;
            }
            return old;
        }
        if i < self.bound() {
            self.dense.resize_with(i + 1, || None);
            self.dense[i] = Some(value);
            let far_old = self.far.remove(&key);
            if far_old.is_none() {
                self.len += 1;
                self.maybe_migrate();
            }
            return far_old;
        }
        let old = self.far.insert(key, value);
        if old.is_none() {
            self.len += 1;
        }
        self.far_end = self.far_end.max(i + 1);
        self.maybe_migrate();
        old
    }

    /// Moves every far entry into the prefix once the occupied range has grown to cover them.
    #[inline]
    fn maybe_migrate(&mut self) {
        if !self.far.is_empty() && self.far_end <= self.bound() {
            self.dense
                .resize_with(self.far_end.max(self.dense.len()), || None);
            for (k, v) in self.far.drain() {
                self.dense[k.raw() as usize] = Some(v);
            }
            self.far.shrink_to_fit();
            self.far_end = 0;
        }
    }

    /// Removes `key`, returning its entity.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let i = key.raw() as usize;
        if i < self.dense.len()
            && let Some(v) = self.dense[i].take()
        {
            self.len -= 1;
            return Some(v);
        }
        let v = self.far.remove(key);
        if v.is_some() {
            self.len -= 1;
        }
        v
    }

    /// Entities held.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// No entities?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Slots allocated (prefix and map), for memory estimates.
    pub fn capacity(&self) -> usize {
        self.dense.capacity() + self.far.capacity()
    }

    /// Drops trailing holes and spare capacity.
    pub fn shrink_to_fit(&mut self) {
        while matches!(self.dense.last(), Some(None)) {
            self.dense.pop();
        }
        self.dense.shrink_to_fit();
        self.far.shrink_to_fit();
    }

    /// Removes everything.
    pub fn clear(&mut self) {
        self.dense.clear();
        self.far.clear();
        self.len = 0;
        self.far_end = 0;
    }

    /// Entities in id order (far-out ids after the prefix, unordered).
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.dense.iter().flatten().chain(self.far.values())
    }

    /// Entities, mutable.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.dense.iter_mut().flatten().chain(self.far.values_mut())
    }

    /// Keys in id order (far-out ids after the prefix, unordered).
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.dense
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|_| K::from_raw(i as u64)))
            .chain(self.far.keys().copied())
    }

    /// Keys and entities in id order.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.dense
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (K::from_raw(i as u64), v)))
            .chain(self.far.iter().map(|(k, v)| (*k, v)))
    }

    /// Consumes the table, yielding the entities.
    pub fn into_values(self) -> impl Iterator<Item = V> {
        self.dense
            .into_iter()
            .flatten()
            .chain(self.far.into_values())
    }

    /// One past the highest id in the prefix: dense per-entity arrays size themselves by it.
    pub fn prefix_len(&self) -> usize {
        self.dense.len()
    }
}

impl<K: IdKey, V> std::ops::Index<&K> for IdTable<K, V> {
    type Output = V;

    fn index(&self, key: &K) -> &V {
        self.get(key).expect("no entity with this id")
    }
}

impl<'a, K: IdKey, V> IntoIterator for &'a IdTable<K, V> {
    type Item = (K, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (K, &'a V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<K: IdKey, V: 'static> IntoIterator for IdTable<K, V> {
    type Item = (K, V);
    type IntoIter = Box<dyn Iterator<Item = (K, V)>>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(
            self.dense
                .into_iter()
                .enumerate()
                .filter_map(|(i, v)| v.map(|v| (K::from_raw(i as u64), v)))
                .chain(self.far),
        )
    }
}

/// Bitset words a set may hold: twice the occupied range in words (the bitmap so far, the
/// members, or the expected count), plus slack. A sparse set over a wide id range stays in the
/// hash set rather than becoming a near-empty bitmap.
fn word_bound(words: usize, len: usize, hint_words: usize) -> usize {
    words
        .max(len)
        .max(hint_words)
        .saturating_mul(2)
        .saturating_add(256)
}

/// A set of ids: a bitset over the id prefix, a hash set for far-out ids. Iteration over the
/// prefix is in id order.
#[derive(Debug, Clone)]
pub struct IdSet<K: IdKey> {
    bits: Vec<u64>,
    far: FxHashSet<K>,
    len: usize,
    /// Words the expected ids span, from the constructor.
    hint_words: usize,
    /// One past the highest word an id in the hash set falls in.
    far_end: usize,
}

impl<K: IdKey> Default for IdSet<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: IdKey> IdSet<K> {
    /// An empty set.
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// An empty set expecting ids in a prefix of about `n`.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            bits: Vec::new(),
            far: FxHashSet::default(),
            len: 0,
            hint_words: n.div_ceil(64),
            far_end: 0,
        }
    }

    #[inline]
    fn bound(&self) -> usize {
        word_bound(self.bits.len(), self.len, self.hint_words)
    }

    /// Adds `key`; `true` if it was absent.
    pub fn insert(&mut self, key: K) -> bool {
        let i = key.raw() as usize;
        let (w, b) = (i / 64, i % 64);
        if w < self.bits.len() {
            let mask = 1u64 << b;
            if self.bits[w] & mask != 0 {
                return false;
            }
            self.bits[w] |= mask;
            // the far set may hold it from before the bitmap reached this id
            if !self.far.is_empty() && self.far.remove(&key) {
                return false;
            }
            self.len += 1;
            self.maybe_migrate();
            return true;
        }
        if w < self.bound() {
            self.bits.resize(w + 1, 0);
            self.bits[w] |= 1u64 << b;
            if !self.far.is_empty() && self.far.remove(&key) {
                return false;
            }
            self.len += 1;
            self.maybe_migrate();
            return true;
        }
        let fresh = self.far.insert(key);
        if fresh {
            self.len += 1;
        }
        self.far_end = self.far_end.max(w + 1);
        self.maybe_migrate();
        fresh
    }

    /// Moves every far id into the bitmap once the occupied range has grown to cover them.
    #[inline]
    fn maybe_migrate(&mut self) {
        if !self.far.is_empty() && self.far_end <= self.bound() {
            self.bits.resize(self.far_end.max(self.bits.len()), 0);
            for k in self.far.drain() {
                let i = k.raw() as usize;
                self.bits[i / 64] |= 1u64 << (i % 64);
            }
            self.far.shrink_to_fit();
            self.far_end = 0;
        }
    }

    /// Removes `key`; `true` if it was present.
    pub fn remove(&mut self, key: &K) -> bool {
        let i = key.raw() as usize;
        let (w, b) = (i / 64, i % 64);
        if w < self.bits.len() {
            let mask = 1u64 << b;
            if self.bits[w] & mask != 0 {
                self.bits[w] &= !mask;
                self.len -= 1;
                return true;
            }
        }
        let had = self.far.remove(key);
        if had {
            self.len -= 1;
        }
        had
    }

    /// Is `key` a member?
    #[inline]
    pub fn contains(&self, key: &K) -> bool {
        let i = key.raw() as usize;
        let (w, b) = (i / 64, i % 64);
        match self.bits.get(w) {
            Some(word) if word & (1u64 << b) != 0 => true,
            _ => !self.far.is_empty() && self.far.contains(key),
        }
    }

    /// Members.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// No members?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Members in id order over the prefix, then far-out ids.
    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.bits
            .iter()
            .enumerate()
            .flat_map(|(w, word)| {
                let mut word = *word;
                std::iter::from_fn(move || {
                    if word == 0 {
                        return None;
                    }
                    let b = word.trailing_zeros() as usize;
                    word &= word - 1;
                    Some(K::from_raw((w * 64 + b) as u64))
                })
            })
            .chain(self.far.iter().copied())
    }

    /// The bitmap over the prefix, when every member is in it (no far-out ids).
    pub fn as_bitmap(&self) -> Option<&[u64]> {
        self.far.is_empty().then_some(self.bits.as_slice())
    }

    /// Members as a vector, in id order over the prefix.
    pub fn to_vec(&self) -> Vec<K> {
        let mut out = Vec::with_capacity(self.len);
        out.extend(self.iter());
        out
    }

    /// Drops spare capacity.
    pub fn shrink_to_fit(&mut self) {
        while matches!(self.bits.last(), Some(0)) {
            self.bits.pop();
        }
        self.bits.shrink_to_fit();
        self.far.shrink_to_fit();
    }

    /// Bytes the set holds.
    pub fn memory_usage(&self) -> usize {
        self.bits.capacity() * 8 + self.far.capacity() * std::mem::size_of::<K>()
    }
}

impl<'a, K: IdKey> IntoIterator for &'a IdSet<K> {
    type Item = K;
    type IntoIter = Box<dyn Iterator<Item = K> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_prefix_and_far() {
        let mut t: IdTable<NodeId, u32> = IdTable::new();
        assert_eq!(t.insert(NodeId(3), 30), None);
        assert_eq!(t.insert(NodeId(0), 0), None);
        assert_eq!(t.insert(NodeId(1 << 40), 7), None);
        assert_eq!(t.len(), 3);
        assert_eq!(t.get(&NodeId(3)), Some(&30));
        assert_eq!(t.get(&NodeId(1 << 40)), Some(&7));
        assert_eq!(t.get(&NodeId(2)), None);
        assert_eq!(t.insert(NodeId(3), 31), Some(30));
        assert_eq!(t.remove(&NodeId(0)), Some(0));
        assert_eq!(t.len(), 2);
        let keys: Vec<u64> = t.keys().map(|k| k.0).collect();
        assert_eq!(keys, vec![3, 1 << 40]);
        t.shrink_to_fit();
        assert_eq!(t.prefix_len(), 4);
    }

    #[test]
    fn table_migrates_hash_order_loads() {
        // ids arrive in an arbitrary order with no size hint: the slab still ends up holding
        // them all, in id order
        let n = 200_000u64;
        let mut t: IdTable<NodeId, u64> = IdTable::new();
        let mut id = 0u64;
        for _ in 0..n {
            id = (id + 104_729) % n;
            t.insert(NodeId(id), id * 2);
        }
        assert_eq!(t.len(), n as usize);
        assert!(t.far.is_empty());
        assert_eq!(t.prefix_len(), n as usize);
        let keys: Vec<u64> = t.keys().map(|k| k.0).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(t.get(&NodeId(12_345)), Some(&24_690));
    }

    #[test]
    fn table_with_hint_is_dense_from_the_start() {
        let mut t: IdTable<EdgeId, u8> = IdTable::with_capacity(1000);
        t.insert(EdgeId(1999), 1);
        assert_eq!(t.prefix_len(), 2000);
        assert!(t.far.is_empty());
        // a sparse table over a wide range stays in the map
        let mut s: IdTable<EdgeId, u8> = IdTable::with_capacity(10);
        for i in 0..10u64 {
            s.insert(EdgeId(i * 1_000_000), 1);
        }
        assert_eq!(s.len(), 10);
        assert_eq!(s.far.len(), 9);
    }

    #[test]
    fn set_orders_the_prefix() {
        let mut s: IdSet<EdgeId> = IdSet::new();
        for id in [70u64, 3, 64, 3, 1 << 50] {
            s.insert(EdgeId(id));
        }
        assert_eq!(s.len(), 4);
        assert!(s.contains(&EdgeId(64)));
        assert!(!s.contains(&EdgeId(65)));
        let v: Vec<u64> = s.to_vec().into_iter().map(|e| e.0).collect();
        assert_eq!(&v[..3], &[3, 64, 70]);
        assert!(s.remove(&EdgeId(64)));
        assert!(!s.remove(&EdgeId(64)));
        assert!(s.remove(&EdgeId(1 << 50)));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn set_migrates_hash_order_loads() {
        let n = 300_000u64;
        let mut s: IdSet<NodeId> = IdSet::new();
        let mut id = 0u64;
        for _ in 0..n {
            id = (id + 104_729) % n;
            s.insert(NodeId(id));
        }
        assert_eq!(s.len(), n as usize);
        assert!(s.far.is_empty());
        let v = s.to_vec();
        assert_eq!(v.len(), n as usize);
        assert!(v.windows(2).all(|w| w[0].0 < w[1].0));
        let mut h: IdSet<NodeId> = IdSet::with_capacity(n as usize);
        h.insert(NodeId(n - 1));
        assert!(h.far.is_empty());
    }
}
