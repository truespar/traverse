//! Morsel sizing: a source is cut into enough morsels for the workers to balance by stealing,
//! each holding about the same work, where work counts the ids plus the adjacency the first
//! expansion above the source has to visit.

use std::ops::Range;

/// Work units (ids or neighbors) a morsel aims to hold.
pub(super) const TARGET_WORK: usize = 64 * 1024;
/// Ids a morsel holds at least (unless the source is smaller).
const MIN_IDS: usize = 1024;
/// Ids a morsel holds at most, whatever their work.
const MAX_IDS: usize = 64 * 1024;

/// `n` items in `m` contiguous ranges of about equal size.
pub(super) fn even_ranges(n: usize, m: usize) -> Vec<Range<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let m = m.clamp(1, n);
    let size = n.div_ceil(m).max(1);
    (0..n).step_by(size).map(|s| s..(s + size).min(n)).collect()
}

/// Contiguous ranges over `weights` holding about `sum / m` work each, cut by work rather than
/// count so a few hubs don't land in one morsel.
pub(super) fn weighted_ranges(weights: &[u32], m: usize) -> Vec<Range<usize>> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    if m <= 1 {
        return std::iter::once(0..n).collect();
    }
    let total: usize = weights.iter().map(|&w| w as usize).sum();
    let target = total.div_ceil(m).max(1);
    let mut out = Vec::with_capacity(m);
    let mut start = 0usize;
    let mut acc = 0usize;
    for (i, &w) in weights.iter().enumerate() {
        acc += w as usize;
        let count = i + 1 - start;
        if (acc >= target && count >= MIN_IDS.min(n)) || count >= MAX_IDS {
            out.push(start..i + 1);
            start = i + 1;
            acc = 0;
        }
    }
    if start < n {
        out.push(start..n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_ranges_cover_everything() {
        let r = even_ranges(10, 3);
        assert_eq!(r, vec![0..4, 4..8, 8..10]);
        assert_eq!(even_ranges(0, 3), Vec::<Range<usize>>::new());
        assert_eq!(even_ranges(2, 8), vec![0..1, 1..2]);
    }

    #[test]
    fn weighted_ranges_cut_by_work() {
        // one hub carries most of the work: it ends a morsel on its own side
        let mut w = vec![1u32; 4096];
        w[100] = 100_000;
        let r = weighted_ranges(&w, 4);
        assert_eq!(r.first().unwrap().start, 0);
        assert_eq!(r.last().unwrap().end, 4096);
        for pair in r.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert!(r.len() >= 2);
    }
}
