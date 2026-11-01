//! Dense bitset indexed by `NodeId.0`: the projection's node filter and the visited/frontier set in
//! BFS-family algorithms. `Vec<u64>`, ~1 bit per node, `contains` is a single bit test. Kept small
//! and inlinable instead of pulling in `fixedbitset` or `bitvec`.

use traverse_core::graph::types::NodeId;

/// Dense bitset over `NodeId.0`; capacity fixed at construction.
#[derive(Debug, Clone)]
pub struct NodeMask {
    words: Vec<u64>,
    capacity: usize,
}

const BITS_PER_WORD: usize = 64;

impl NodeMask {
    /// Empty mask for ids `0..capacity`.
    pub fn with_capacity(capacity: usize) -> Self {
        let words = capacity.div_ceil(BITS_PER_WORD);
        Self {
            words: vec![0u64; words],
            capacity,
        }
    }

    /// Sets the bit at `idx`; no-op if `idx >= capacity`.
    #[inline]
    pub fn set(&mut self, idx: usize) {
        if idx >= self.capacity {
            return;
        }
        let w = idx / BITS_PER_WORD;
        let b = idx % BITS_PER_WORD;
        self.words[w] |= 1u64 << b;
    }

    /// Tests the bit at `idx`; `false` when out of range.
    #[inline(always)]
    pub fn contains(&self, idx: usize) -> bool {
        if idx >= self.capacity {
            return false;
        }
        let w = idx / BITS_PER_WORD;
        let b = idx % BITS_PER_WORD;
        (self.words[w] & (1u64 << b)) != 0
    }

    /// [`Self::contains`] for a node id.
    #[inline(always)]
    pub fn contains_node(&self, node: NodeId) -> bool {
        self.contains(node.0 as usize)
    }

    /// Number of set bits.
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Indices of set bits.
    pub fn iter_ones(&self) -> NodeMaskIter<'_> {
        NodeMaskIter {
            words: &self.words,
            word_idx: 0,
            current: self.words.first().copied().unwrap_or(0),
            capacity: self.capacity,
        }
    }
}

/// Set positions of a [`NodeMask`], ascending.
pub struct NodeMaskIter<'a> {
    words: &'a [u64],
    word_idx: usize,
    current: u64,
    capacity: usize,
}

impl Iterator for NodeMaskIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                let idx = self.word_idx * BITS_PER_WORD + bit;
                // clear the lowest set bit
                self.current &= self.current - 1;
                if idx < self.capacity {
                    return Some(idx);
                }
                // skip out-of-range bits in the last word's tail
                continue;
            }
            self.word_idx += 1;
            if self.word_idx >= self.words.len() {
                return None;
            }
            self.current = self.words[self.word_idx];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_contains() {
        let mut m = NodeMask::with_capacity(200);
        assert!(!m.contains(42));
        m.set(42);
        assert!(m.contains(42));
        assert_eq!(m.count_ones(), 1);
    }

    #[test]
    fn iter_ones_visits_set_bits_in_order() {
        let mut m = NodeMask::with_capacity(200);
        for i in [3, 63, 64, 65, 127, 128, 199] {
            m.set(i);
        }
        let collected: Vec<usize> = m.iter_ones().collect();
        assert_eq!(collected, vec![3, 63, 64, 65, 127, 128, 199]);
    }
}
