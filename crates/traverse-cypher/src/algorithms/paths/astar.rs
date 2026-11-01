//! A\* shortest path, single source and target, with a heuristic.
//!
//! # Algorithm
//!
//! Dijkstra's priority-queue relaxation keyed by `g(n) + h(n)` instead of
//! `g(n)`; `h(n)` must be admissible (never overestimate the remaining cost).
//!
//! # Heuristic
//!
//! Geographic: `latitudeProperty` / `longitudeProperty` on nodes, great-circle
//! (Haversine) distance to the target in **kilometers**, admissible for edge
//! weights in kilometers or larger (e.g. road networks).
//!
//! With the property missing or zero, A\* becomes Dijkstra, silently: paths
//! are still correct.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_bool, opt_integer, parse_integer, parse_string};
use super::super::projection::AlgorithmGraph;
use super::super::result::{PathRecord, PathResult};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// A* between two nodes, guided by great-circle distance of coordinate properties or by ALT
/// landmarks (`traverse.aStar.*`). Stateless.
pub struct AStar;

/// Parsed `aStar` configuration.
pub struct AStarConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// End node (`targetNodeId`, required).
    pub target: NodeId,
    /// Property with latitude in degrees (`latitudeProperty`, default `latitude`).
    pub latitude_key: String,
    /// Property with longitude in degrees (`longitudeProperty`, default `longitude`).
    pub longitude_key: String,
    /// Use ALT (A*, Landmarks, Triangle inequality): `landmark_count` landmarks with precomputed
    /// Dijkstra each, heuristic `max_L |d(L, t) - d(L, u)|`; admissible and consistent on
    /// non-geographic graphs.
    pub use_landmarks: bool,
    /// Landmarks for ALT (`landmarkCount`, default 4, >= 1).
    pub landmark_count: usize,
}

impl GraphAlgorithm for AStar {
    type Config = AStarConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "aStar",
            display_name: "A* Shortest Path",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Source -> target weighted shortest path. Default heuristic is Haversine \
                 (great-circle) over latitudeProperty + longitudeProperty. Set \
                 `useLandmarks: true` for an ALT heuristic - picks `landmarkCount` \
                 landmarks, precomputes Dijkstra from each, and uses the triangle-\
                 inequality lower bound. ALT works on any graph (no coordinates needed) \
                 and is the right choice for non-geographic networks.",
            params: vec![
                ConfigParam {
                    key: "sourceNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Required. Source node.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "targetNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Required. Target node.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "latitudeProperty",
                    ty: ConfigParamType::String,
                    default: CypherValue::String("latitude".into()),
                    description: "Node property holding latitude in degrees.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "longitudeProperty",
                    ty: ConfigParamType::String,
                    default: CypherValue::String("longitude".into()),
                    description: "Node property holding longitude in degrees.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "useLandmarks",
                    ty: ConfigParamType::Bool,
                    default: CypherValue::Bool(false),
                    description: "Use ALT (A*-Landmarks-Triangle) heuristic instead of Haversine. \
                         Required for non-geographic graphs.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "landmarkCount",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(4),
                    description: "Number of landmarks for ALT. More landmarks -> tighter heuristic, \
                         higher precompute cost.",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: OutputKind::Path.default_columns().unwrap(),
            stats_columns: OutputKind::Path.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<AStarConfig, AlgorithmError> {
        let source_val = params
            .get("sourceNodeId")
            .ok_or_else(|| AlgorithmError::Config("sourceNodeId is required".into()))?;
        let target_val = params
            .get("targetNodeId")
            .ok_or_else(|| AlgorithmError::Config("targetNodeId is required".into()))?;
        let source = parse_integer("sourceNodeId", source_val).map_err(AlgorithmError::from)?;
        let target = parse_integer("targetNodeId", target_val).map_err(AlgorithmError::from)?;
        let latitude_key = params
            .get("latitudeProperty")
            .map(|v| parse_string("latitudeProperty", v))
            .transpose()
            .map_err(AlgorithmError::from)?
            .unwrap_or_else(|| "latitude".into());
        let longitude_key = params
            .get("longitudeProperty")
            .map(|v| parse_string("longitudeProperty", v))
            .transpose()
            .map_err(AlgorithmError::from)?
            .unwrap_or_else(|| "longitude".into());
        let use_landmarks =
            opt_bool(params, "useLandmarks", false).map_err(AlgorithmError::from)?;
        let landmark_count =
            opt_integer(params, "landmarkCount", 4).map_err(AlgorithmError::from)? as usize;
        Ok(AStarConfig {
            source: NodeId(source as u64),
            target: NodeId(target as u64),
            latitude_key,
            longitude_key,
            use_landmarks,
            landmark_count: landmark_count.max(1),
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &AStarConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let s_idx = cfg.source.0 as usize;
        let t_idx = cfg.target.0 as usize;
        if s_idx >= cap || t_idx >= cap {
            return Ok(PathResult { paths: Vec::new() });
        }

        // heuristic choice:
        //  - useLandmarks=true -> ALT: K Dijkstras from K landmarks up front, then
        //    h(u, t) = max_L |d(L, t) - d(L, u)|
        //  - else (default) -> Haversine over (latitude, longitude); 0 without
        //    coordinates, i.e. Dijkstra
        // mutually exclusive; with coordinates and a wish for ALT, run both and compare
        let landmarks: Option<Landmarks> = if cfg.use_landmarks {
            Some(landmarks_precompute(
                graph,
                cfg.landmark_count,
                cap,
                cancel,
            )?)
        } else {
            None
        };

        let view = graph.graph();
        let (lat, lng, target_lat, target_lng) = if cfg.use_landmarks {
            (Vec::new(), Vec::new(), f64::NAN, f64::NAN)
        } else {
            let mut lat: Vec<f64> = vec![f64::NAN; cap];
            let mut lng: Vec<f64> = vec![f64::NAN; cap];
            for v in graph.active_nodes() {
                lat[v.0 as usize] = read_coord(view, v, &cfg.latitude_key);
                lng[v.0 as usize] = read_coord(view, v, &cfg.longitude_key);
            }
            let tl = lat[t_idx];
            let tg = lng[t_idx];
            (lat, lng, tl, tg)
        };
        let heuristic = |u_idx: usize| -> f64 {
            if let Some(lm) = &landmarks {
                lm.heuristic(u_idx, t_idx)
            } else {
                haversine_km(lat[u_idx], lng[u_idx], target_lat, target_lng)
            }
        };

        const SENTINEL: u32 = u32::MAX;
        let mut g_score: Vec<f64> = vec![f64::INFINITY; cap];
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
        g_score[s_idx] = 0.0;

        let mut heap: BinaryHeap<Reverse<(OrderedF64, NodeId)>> = BinaryHeap::new();
        heap.push(Reverse((OrderedF64(heuristic(s_idx)), cfg.source)));

        while let Some(Reverse((OrderedF64(_f), u))) = heap.pop() {
            if u == cfg.target {
                break;
            }
            let ui = u.0 as usize;
            cancel.check()?;
            if graph.is_unit_weighted()
                && let Some(slice) = graph.outgoing_adj_slice(u)
            {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi >= cap {
                        continue;
                    }
                    let tentative = g_score[ui] + 1.0;
                    if tentative < g_score[vi] {
                        g_score[vi] = tentative;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = ep.edge_id().0;
                        let h = heuristic(vi);
                        heap.push(Reverse((OrderedF64(tentative + h), NodeId(vi as u64))));
                    }
                }
                continue;
            }
            let mut err: Option<AlgorithmError> = None;
            graph.for_each_out(u, &mut |v, eid, w| {
                if err.is_some() {
                    return;
                }
                if w < 0.0 {
                    err = Some(AlgorithmError::Other(format!(
                        "A* requires non-negative edge weights; saw {w}"
                    )));
                    return;
                }
                let vi = v.0 as usize;
                if vi >= cap {
                    return;
                }
                let tentative = g_score[ui] + w;
                if tentative < g_score[vi] {
                    g_score[vi] = tentative;
                    pred[vi] = u.0 as u32;
                    pred_edge[vi] = eid;
                    let h = heuristic(vi);
                    heap.push(Reverse((OrderedF64(tentative + h), v)));
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }

        if !g_score[t_idx].is_finite() {
            return Ok(PathResult { paths: Vec::new() });
        }

        // reconstruct target -> source
        let mut nodes_rev: Vec<NodeId> = Vec::new();
        let mut edges_rev: Vec<EdgeId> = Vec::new();
        let mut costs_rev: Vec<f64> = Vec::new();
        let mut cur = cfg.target;
        loop {
            nodes_rev.push(cur);
            costs_rev.push(g_score[cur.0 as usize]);
            if cur == cfg.source {
                break;
            }
            let p = pred[cur.0 as usize];
            if p == SENTINEL {
                return Ok(PathResult { paths: Vec::new() });
            }
            edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
            cur = NodeId(p as u64);
        }
        nodes_rev.reverse();
        edges_rev.reverse();
        costs_rev.reverse();
        Ok(PathResult {
            paths: vec![PathRecord {
                source: cfg.source,
                target: cfg.target,
                total_cost: g_score[t_idx],
                node_ids: nodes_rev,
                edge_ids: edges_rev,
                costs: costs_rev,
            }],
        })
    }
}

fn read_coord(view: &dyn traverse_core::tx::view::GraphView, node: NodeId, key: &str) -> f64 {
    match view.get_node_property(node, key) {
        Ok(Some(PropertyValue::Float(f))) => f,
        Ok(Some(PropertyValue::Int(i))) => i as f64,
        _ => f64::NAN,
    }
}

/// Great-circle distance in km; 0 if either coordinate pair is missing (NaN), keeping the heuristic
/// permissive (A\* becomes Dijkstra).
fn haversine_km(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    if lat1.is_nan() || lng1.is_nan() || lat2.is_nan() || lng2.is_nan() {
        return 0.0;
    }
    let r = 6371.0; // Earth radius, km
    let to_rad = std::f64::consts::PI / 180.0;
    let dlat = (lat2 - lat1) * to_rad;
    let dlng = (lng2 - lng1) * to_rad;
    let a = (dlat * 0.5).sin().powi(2)
        + (lat1 * to_rad).cos() * (lat2 * to_rad).cos() * (dlng * 0.5).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    r * c
}

#[derive(Debug, Clone, Copy)]
struct OrderedF64(f64);
impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// ALT precompute: K landmarks, a Dijkstra from each to every node, stored as a
/// row-major `K x cap` matrix. Query heuristic `h(u, t) = max_L |d(L, t) - d(L, u)|`
/// is admissible and consistent for non-negative weights. Landmarks are the
/// highest-degree nodes (well connected, so tighter bounds); deterministic.
struct Landmarks {
    cap: usize,
    /// Row-major: `dists[l * cap + node] = d(landmark_l, node)`.
    dists: Vec<f64>,
    n_landmarks: usize,
}

impl Landmarks {
    #[inline]
    fn heuristic(&self, u_idx: usize, t_idx: usize) -> f64 {
        let mut best: f64 = 0.0;
        for l in 0..self.n_landmarks {
            let row = l * self.cap;
            let du = self.dists[row + u_idx];
            let dt = self.dists[row + t_idx];
            // u unreachable from L (row is infinite): skip this landmark
            if du.is_finite() && dt.is_finite() {
                let diff = (dt - du).abs();
                if diff > best {
                    best = diff;
                }
            }
        }
        best
    }
}

fn landmarks_precompute(
    graph: &dyn AlgorithmGraph,
    k: usize,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Landmarks, AlgorithmError> {
    // K landmarks: highest-degree active nodes (stable, reproducible)
    let active = graph.active_nodes();
    let mut by_degree: Vec<(u32, NodeId)> =
        active.iter().map(|&v| (graph.out_degree(v), v)).collect();
    by_degree.sort_by_key(|d| std::cmp::Reverse(d.0));
    let chosen: Vec<NodeId> = by_degree
        .into_iter()
        .take(k.min(active.len()))
        .map(|(_, v)| v)
        .collect();
    let n_landmarks = chosen.len();

    // Dijkstra per landmark, serially: heap state isn't easy to share and K <= 8 typically
    let mut dists: Vec<f64> = vec![f64::INFINITY; n_landmarks * cap];
    for (li, &landmark) in chosen.iter().enumerate() {
        cancel.check()?;
        let base = li * cap;
        dists[base + landmark.0 as usize] = 0.0;
        let mut heap: BinaryHeap<Reverse<(OrderedF64, NodeId)>> = BinaryHeap::new();
        heap.push(Reverse((OrderedF64(0.0), landmark)));
        while let Some(Reverse((OrderedF64(d_u), u))) = heap.pop() {
            let ui = u.0 as usize;
            if d_u > dists[base + ui] {
                continue;
            }
            if graph.is_unit_weighted()
                && let Some(slice) = graph.outgoing_adj_slice(u)
            {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    let nd = d_u + 1.0;
                    if nd < dists[base + vi] {
                        dists[base + vi] = nd;
                        heap.push(Reverse((OrderedF64(nd), NodeId(vi as u64))));
                    }
                }
                continue;
            }
            let mut err: Option<AlgorithmError> = None;
            graph.for_each_out(u, &mut |v, _eid, w| {
                if err.is_some() {
                    return;
                }
                if w < 0.0 {
                    err = Some(AlgorithmError::Other(
                        "Landmark Dijkstra requires non-negative weights".into(),
                    ));
                    return;
                }
                let vi = v.0 as usize;
                let nd = d_u + w;
                if nd < dists[base + vi] {
                    dists[base + vi] = nd;
                    heap.push(Reverse((OrderedF64(nd), v)));
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
    }
    Ok(Landmarks {
        cap,
        dists,
        n_landmarks,
    })
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(AStar);
}
