//! Strongly connected components (Tarjan, 1972).
//!
//! # Algorithm
//!
//! One DFS with two stamps per node:
//! * `index[v]`: discovery order
//! * `lowlink[v]`: lowest index reachable via tree/back edges
//!
//! When `lowlink[v] == index[v]` as `v` finishes, the stack down to `v` is one
//! SCC. Its id is the smallest NodeId in it, like WCC.
//!
//! # Notes
//!
//! * **Iterative DFS**: Tarjan's published pseudocode recurses, which overflows
//!   on deep graphs (long chains); an explicit work stack carries
//!   `(node, edge_iter_state)`.
//! * **No HashMap/HashSet** on the hot path: per-node state in dense `Vec`s by
//!   `NodeId.0`.
//! * **Single-threaded**: Tarjan's DFS order is inherently sequential; parallel
//!   SCC kernels (Hong et al., Slota-Madduri) trade extra work for parallelism
//!   and only pay off on billions of edges.

use std::collections::BTreeMap;

use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeCommunityResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// Strongly connected components by Tarjan's algorithm (`traverse.scc.*`). Stateless.
pub struct Scc;

impl GraphAlgorithm for Scc {
    type Config = ();
    type Output = NodeCommunityResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "scc",
            display_name: "Strongly Connected Components",
            category: Category::Community,
            output_kind: OutputKind::NodeCommunity,
            stability: Stability::Beta,
            description: "Partition nodes into SCCs using Tarjan's iterative DFS. Each component \
                 is identified by the smallest NodeId it contains.",
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
        if nodes.is_empty() {
            return Ok(NodeCommunityResult::new("componentId", vec![u64::MAX; cap]));
        }

        const UNVISITED: i32 = -1;
        let mut index_of: Vec<i32> = vec![UNVISITED; cap];
        let mut lowlink: Vec<i32> = vec![0; cap];
        let mut on_stack: Vec<bool> = vec![false; cap];
        let mut stack: Vec<NodeId> = Vec::new();
        let mut comp: Vec<u64> = vec![u64::MAX; cap];

        let mut next_index: i32 = 0;

        // buffer each node's out-neighbors once at first touch, so the iterative DFS resumes
        // mid-list without re-walking the projection (see `struct Frame`)
        let mut work: Vec<Frame> = Vec::new();

        for &start in &nodes {
            if index_of[start.0 as usize] != UNVISITED {
                continue;
            }
            cancel.check()?;
            // new DFS root
            push_frame(
                graph,
                start,
                &mut work,
                &mut index_of,
                &mut lowlink,
                &mut on_stack,
                &mut stack,
                &mut next_index,
            );

            while let Some(top) = work.last_mut() {
                if top.pos < top.neighbors.len() {
                    let w = top.neighbors.get(top.pos);
                    top.pos += 1;
                    let wi = w.0 as usize;
                    if wi >= cap {
                        continue;
                    }
                    if index_of[wi] == UNVISITED {
                        // tree edge: descend
                        push_frame(
                            graph,
                            w,
                            &mut work,
                            &mut index_of,
                            &mut lowlink,
                            &mut on_stack,
                            &mut stack,
                            &mut next_index,
                        );
                    } else if on_stack[wi] {
                        // back / cross edge to an SCC candidate
                        let v_idx = top.v.0 as usize;
                        if index_of[wi] < lowlink[v_idx] {
                            lowlink[v_idx] = index_of[wi];
                        }
                    }
                } else {
                    // `top.v`'s neighbors exhausted: emit an SCC if it's a root
                    let v = top.v;
                    let v_idx = v.0 as usize;
                    if lowlink[v_idx] == index_of[v_idx] {
                        // pop down to (and including) v; the smallest NodeId popped labels the
                        // component
                        let mut min_id = v.0;
                        let mut popped_start = stack.len();
                        loop {
                            popped_start -= 1;
                            let w = stack[popped_start];
                            if w.0 < min_id {
                                min_id = w.0;
                            }
                            if w == v {
                                break;
                            }
                        }
                        for &w in &stack[popped_start..] {
                            let wi = w.0 as usize;
                            comp[wi] = min_id;
                            on_stack[wi] = false;
                        }
                        stack.truncate(popped_start);
                    }
                    work.pop();
                    // propagate lowlink to the caller
                    if let Some(parent) = work.last_mut() {
                        let p_idx = parent.v.0 as usize;
                        if lowlink[v_idx] < lowlink[p_idx] {
                            lowlink[p_idx] = lowlink[v_idx];
                        }
                    }
                }
            }
        }

        Ok(NodeCommunityResult::new("componentId", comp))
    }
}

fn push_frame<'a>(
    graph: &'a dyn AlgorithmGraph,
    v: NodeId,
    work: &mut Vec<Frame<'a>>,
    index_of: &mut [i32],
    lowlink: &mut [i32],
    on_stack: &mut [bool],
    stack: &mut Vec<NodeId>,
    next_index: &mut i32,
) {
    let i = v.0 as usize;
    index_of[i] = *next_index;
    lowlink[i] = *next_index;
    *next_index += 1;
    on_stack[i] = true;
    stack.push(v);
    // slice fast path without projection filters: store the borrowed `&[AdjEntry]` in the frame, no
    // per-frame `Vec<NodeId>` allocation + copy (a large share of SCC time otherwise)
    let neighbors = if let Some(slice) = graph.outgoing_adj_slice(v) {
        FrameNeighbors::Slice(slice)
    } else {
        let mut n: Vec<NodeId> = Vec::new();
        graph.for_each_out(v, &mut |w, _eid, _weight| n.push(w));
        FrameNeighbors::Owned(n)
    };
    work.push(Frame {
        v,
        neighbors,
        pos: 0,
    });
}

enum FrameNeighbors<'a> {
    Slice(&'a [traverse_core::graph::adjacency::AdjEntry]),
    Owned(Vec<NodeId>),
}

impl<'a> FrameNeighbors<'a> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            FrameNeighbors::Slice(s) => s.len(),
            FrameNeighbors::Owned(v) => v.len(),
        }
    }

    #[inline]
    fn get(&self, idx: usize) -> NodeId {
        match self {
            FrameNeighbors::Slice(s) => NodeId(s[idx].neighbor().0),
            FrameNeighbors::Owned(v) => v[idx],
        }
    }
}

struct Frame<'a> {
    v: NodeId,
    neighbors: FrameNeighbors<'a>,
    pos: usize,
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Scc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn two_cycles_form_two_sccs() {
        // a -> b -> a and c -> d -> e -> c
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        let e = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, a, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();
        g.create_edge(d, e, "T", Default::default()).unwrap();
        g.create_edge(e, c, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Scc.run(&proj, &(), &CancellationToken::none()).unwrap();
        // {a,b} one component, {c,d,e} another
        assert_eq!(r.values[a.0 as usize], r.values[b.0 as usize]);
        assert_eq!(r.values[c.0 as usize], r.values[d.0 as usize]);
        assert_eq!(r.values[d.0 as usize], r.values[e.0 as usize]);
        assert_ne!(r.values[a.0 as usize], r.values[c.0 as usize]);
    }

    #[test]
    fn dag_makes_singletons() {
        // a -> b -> c: no cycles, 3 singletons
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Scc.run(&proj, &(), &CancellationToken::none()).unwrap();
        assert_eq!(r.values[a.0 as usize], a.0);
        assert_eq!(r.values[b.0 as usize], b.0);
        assert_eq!(r.values[c.0 as usize], c.0);
    }
}
