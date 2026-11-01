//! Plan executor: runs a [`LogicalPlan`] against a `dyn GraphView`.
//!
//! `run` drives a plan and shapes its result; operators call back into the pieces here:
//! expression evaluation (`eval`, `compare`), projection, expansion and shortest paths,
//! aggregation, writes and their constraint checks. Subqueries and pattern comprehensions that the
//! operator path can't take run on the record interpreter (`interp`).
//!
//! [`LogicalPlan`]: crate::planner::LogicalPlan

mod aggregate;
mod bound;
mod compare;
mod comprehension;
mod constraints;
mod context;
mod describe;
mod eval;
mod expand;
mod explain;
mod interp;
mod plan_cache;
mod project;
mod run;
mod shortest;
mod subquery;
mod values;
mod write;

use std::collections::HashMap;

use crate::types::CypherValue;

pub use context::{
    QueryStats, QueryType, end_execution, get_query_param, get_query_param_keys, get_query_stats,
    reset_query_stats,
};
pub use describe::{classify_plan, plan_columns};
pub use eval::eval_expr;
pub use explain::plan_to_cypher_value;
pub use run::{
    CypherResult, CypherResultFooter, CypherResultStream, LazyResult, QueryExecution,
    QueryResultLazy, execute_plan, execute_plan_lazy, execute_plan_streaming,
};
pub use values::{edge_to_cypher_value, materialize_value, node_to_cypher_value};

pub(crate) use aggregate::{collect_graph_parts, exec_aggregate};
pub(crate) use bound::{
    bound_capture_armed, clone_bound_handle, collect_entity_slots, flush_bound, set_bound_handle,
};
pub(crate) use compare::{compare_values, eval_comparison, proc_values_equal};
pub(crate) use comprehension::current_subquery_seed;
pub(crate) use constraints::{check_all_edge_constraints, check_all_node_constraints};
pub(crate) use context::{cached_regex, inc_stat, reset_pointer_caches};
pub(crate) use describe::{plan_is_mutating, projection_name};
pub(crate) use eval::{eval_integer_expr, get_property, is_truthy};
pub(crate) use expand::{
    collect_prior_edge_ids, exec_expand, exec_expand_frames, exec_streaming_expand_chain,
    extract_expand_chain, get_node_id_frame, resolve_edge_type_filter,
};
pub(crate) use interp::{exec_bind_path_endpoints, exec_subquery, get_node_id};
pub(crate) use project::{exec_project_items, exec_scan_filter_project};
pub(crate) use shortest::{
    bfs_all_shortest, bfs_k_groups, bfs_k_shortest, bfs_single_shortest, dijkstra_single_shortest,
};
pub(crate) use values::{
    JoinKey, cypher_to_join_key, cypher_value_to_index_key, cypher_value_to_property_map,
    dedup_frames, eval_property_map,
};
pub(crate) use write::{exec_merge, exec_remove_item, exec_set_item};

/// One result row.
pub type Record = HashMap<String, CypherValue>;

/// Ids at or above this are VIRTUAL entities (APOC vNode / vRelationship): pure values that never
/// resolve against the graph.
pub(crate) const VIRTUAL_ID_BASE: u64 = 1 << 62;
