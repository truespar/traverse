//! Weakly connected components via union-find.
//!
//! # Algorithm
//!
//! **Afforest** (Sutton, Ben-Nun, Barak, IPDPS 2018), exploiting the heavy-tailed
//! degrees of real social/web graphs: a cheap sampling step links most nodes
//! into the dominant component, then a sweep skips nodes already in it.
//!
//! 1. **Sample**: link each node with its first `r` neighbors (`r = 2`); the
//!    giant component is then almost complete.
//! 2. **Find the dominant root**: the largest component's representative.
//! 3. **Linear sweep**: for each node *not* in the dominant component, union
//!    with all neighbors. Edges inside the dominant component are skipped
//!    (`find_compress` gives the same root both ways).
//! 4. **Final compression**: every node's parent becomes its root.
//!
//! On graphs with one giant component the sample phase already places nearly all
//! nodes, so phase 3 only walks the stragglers' edges; clearly faster than a
//! plain Shiloach-Vishkin union over all edges.
//!
//! # Output
//!
//! `OutputKind::NodeCommunity`: a `u64` per node, the smallest NodeId in the component.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeCommunityResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// Weakly connected components by union-find (`traverse.wcc.*`). Stateless.
pub struct Wcc;

impl GraphAlgorithm for Wcc {
    type Config = ();
    type Output = NodeCommunityResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "wcc",
            display_name: "Weakly Connected Components",
            category: Category::Community,
            output_kind: OutputKind::NodeCommunity,
            stability: Stability::Beta,
            description: "Assign each node the id of its weakly connected component (smallest NodeId in the component).",
            params: Vec::new(),
            output_columns: OutputKind::NodeCommunity.default_columns().unwrap(),
            stats_columns: OutputKind::NodeCommunity.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeCommunity.default_write_columns().unwrap(),
        }
    }

    fn parse_config(&self, _params: &BTreeMap<String, CypherValue>) -> Result<(), AlgorithmError> {
        Ok(())
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        _cfg: &(),
        cancel: &CancellationToken,
    ) -> Result<NodeCommunityResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();

        // parent[i] = representative of node i, each starting as its own singleton; inactive nodes
        // keep `u64::MAX` and stream as unassigned
        let parent: Vec<AtomicU64> = (0..cap as u64).map(AtomicU64::new).collect();
        let mut inactive = vec![true; cap];
        for &n in &nodes {
            inactive[n.0 as usize] = false;
        }

        cancel.check()?;

        let is_undirected = graph.is_undirected();

        // -- phase 1: SAMPLE, union with each node's first `SAMPLE_R` neighbors. On
        // heavy-tailed graphs this builds nearly the whole giant component in
        // ~O(N * SAMPLE_R) instead of O(M).
        //
        // NO `v_idx < n` filter here, even undirected: it's right for a full pass (each
        // edge seen from both ends) but breaks sampling, since high-id nodes whose first
        // R neighbors are all lower-id would link to nothing. Each node samples its OWN
        // first R; mutual sampling at most doubles the unions, and `union_atomic`
        // short-circuits merged pairs.
        const SAMPLE_R: usize = 2;
        nodes.par_iter().for_each(|&v| {
            let v_idx = v.0;
            let mut sampled = 0usize;
            if let Some(slice) = graph.outgoing_adj_slice(v) {
                for ep in slice {
                    if sampled >= SAMPLE_R {
                        break;
                    }
                    let n = ep.neighbor().0;
                    if n == v_idx {
                        continue; // skip self-loops
                    }
                    union_atomic(&parent, v_idx, n);
                    sampled += 1;
                }
            } else {
                let mut count: usize = 0;
                graph.for_each_out(v, &mut |u, _eid, _w| {
                    if count >= SAMPLE_R {
                        return;
                    }
                    if u.0 == v_idx {
                        return;
                    }
                    union_atomic(&parent, v_idx, u.0);
                    count += 1;
                });
            }
            let _ = sampled;
        });

        cancel.check()?;

        // -- phase 2: find the dominant (largest) component; path-compress every node first so
        // parent[] maps straight to roots
        nodes.par_iter().for_each(|&v| {
            let _ = find_compress(&parent, v.0);
        });
        let dominant_root = dominant_component(&parent, &nodes);

        cancel.check()?;

        // -- phase 3: LINEAR SWEEP, union the remaining edges of nodes NOT in the dominant
        // component. Most edges of real social graphs already sit inside the dominant tree, so
        // those nodes skip their adjacency; stragglers walk every edge
        nodes.par_iter().for_each(|&v| {
            let v_idx = v.0;
            if parent[v_idx as usize].load(Ordering::Acquire) == dominant_root {
                return;
            }
            if let Some(slice) = graph.outgoing_adj_slice(v) {
                if is_undirected {
                    for ep in slice {
                        let n = ep.neighbor().0;
                        if v_idx < n {
                            union_atomic(&parent, v_idx, n);
                        }
                    }
                } else {
                    for ep in slice {
                        union_atomic(&parent, v_idx, ep.neighbor().0);
                    }
                }
            } else if is_undirected {
                graph.for_each_out(v, &mut |u, _eid, _w| {
                    if v_idx < u.0 {
                        union_atomic(&parent, v_idx, u.0);
                    }
                });
            } else {
                graph.for_each_out(v, &mut |u, _eid, _w| {
                    union_atomic(&parent, v_idx, u.0);
                });
            }
        });

        cancel.check()?;

        // -- phase 4: final compression, every parent becomes its root (cheap, parallel)
        nodes.par_iter().for_each(|&v| {
            let _ = find_compress(&parent, v.0);
        });

        // materialize; inactive slots stay u64::MAX so the stream drops them
        let mut out = vec![u64::MAX; cap];
        for &n in &nodes {
            out[n.0 as usize] = find_compress(&parent, n.0);
        }
        Ok(NodeCommunityResult::new("componentId", out))
    }
}

/// Most frequent root among active nodes. After phase 2's full compression `parent[i]` is the root,
/// so a root histogram gives component sizes. `u64::MAX` (no skipping in phase 3) if nothing is
/// active.
fn dominant_component(parent: &[AtomicU64], nodes: &[traverse_core::graph::types::NodeId]) -> u64 {
    use rustc_hash::FxHashMap;
    // parallel fold-reduce of per-worker count maps; a cap-sized histogram per worker would be
    // large and mostly empty, the map is sized to the components present
    let counts: FxHashMap<u64, u32> = nodes
        .par_iter()
        .fold(FxHashMap::<u64, u32>::default, |mut acc, &v| {
            let r = parent[v.0 as usize].load(Ordering::Acquire);
            *acc.entry(r).or_insert(0) += 1;
            acc
        })
        .reduce(FxHashMap::<u64, u32>::default, |mut a, b| {
            for (k, v) in b {
                *a.entry(k).or_insert(0) += v;
            }
            a
        });
    counts
        .into_iter()
        .max_by_key(|&(_, c)| c)
        .map(|(r, _)| r)
        .unwrap_or(u64::MAX)
}

// -- Lock-free union-find ---------------------------------------------

/// Path-compresses and returns the root of `x`; `compare_exchange_weak` lets racing compressions
/// converge on the same root without losing updates.
#[inline]
fn find_compress(parent: &[AtomicU64], x: u64) -> u64 {
    let mut cur = x;
    loop {
        let p = parent[cur as usize].load(Ordering::Acquire);
        if p == cur {
            return cur;
        }
        let gp = parent[p as usize].load(Ordering::Acquire);
        if gp == p {
            // p is a root: splice cur straight to it
            let _ = parent[cur as usize].compare_exchange_weak(
                p,
                gp,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return p;
        }
        // halving: point cur at its grandparent, continue
        let _ =
            parent[cur as usize].compare_exchange_weak(p, gp, Ordering::AcqRel, Ordering::Acquire);
        cur = gp;
    }
}

/// Lock-free union by smallest root.
#[inline]
fn union_atomic(parent: &[AtomicU64], mut x: u64, mut y: u64) {
    loop {
        x = find_compress(parent, x);
        y = find_compress(parent, y);
        if x == y {
            return;
        }
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        // point `hi` at `lo`; a failed CAS means hi's parent changed, retry with fresh roots
        match parent[hi as usize].compare_exchange(hi, lo, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(_) => continue,
        }
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Wcc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    fn run_wcc(g: &Graph) -> Vec<u64> {
        let view: &(dyn GraphView + Sync) = g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        Wcc.run(&proj, &(), &CancellationToken::none())
            .unwrap()
            .values
    }

    #[test]
    fn two_disjoint_pairs() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();
        let r = run_wcc(&g);
        assert_eq!(r[a.0 as usize], r[b.0 as usize]);
        assert_eq!(r[c.0 as usize], r[d.0 as usize]);
        assert_ne!(r[a.0 as usize], r[c.0 as usize]);
    }

    #[test]
    fn single_chain_is_one_component() {
        let mut g = Graph::new();
        let n: Vec<_> = (0..10)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..9 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let r = run_wcc(&g);
        let root = r[n[0].0 as usize];
        for ni in &n {
            assert_eq!(r[ni.0 as usize], root);
        }
    }

    #[test]
    fn isolated_node_forms_singleton() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let isolated = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        let r = run_wcc(&g);
        assert_eq!(r[a.0 as usize], r[b.0 as usize]);
        // the singleton has its own component, apart from a/b
        assert_ne!(r[isolated.0 as usize], r[a.0 as usize]);
    }

    #[test]
    fn merging_via_undirected_reachability() {
        // a -> b, c -> b: a and c share a component since WCC ignores direction
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(c, b, "T", Default::default()).unwrap();
        let r = run_wcc(&g);
        assert_eq!(r[a.0 as usize], r[c.0 as usize]);
        assert_eq!(r[a.0 as usize], r[b.0 as usize]);
    }
}
