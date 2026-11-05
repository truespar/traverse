//! Plan execution: the result types and entry points over the vectorized engine
//! ([`crate::vex`]), and what the engine and the procedures share: mutation counters, query
//! classification, constraint checks, shortest-path kernels and bound-entity capture.

mod bound;
mod constraints;
mod context;
mod describe;
mod run;
mod shortest;

pub use context::{
    QueryStats, QueryType, end_execution, get_query_param, get_query_param_keys, get_query_stats,
    reset_query_stats,
};
pub use describe::{classify_plan, plan_columns};
pub use run::{
    CypherResult, CypherResultFooter, CypherResultStream, LaneRow, LazyResult, QueryExecution,
    QueryResultLazy, classify, execute_physical, execute_physical_lazy, execute_physical_streaming,
    execute_plan, plan_to_cypher_value,
};

pub use crate::vex::boundary::eval_expr;
pub use crate::vex::values::{edge_to_cypher_value, materialize_value, node_to_cypher_value};

pub(crate) use bound::{
    BoundHandle, bound_capture_armed, clone_bound_handle, flush_bound, set_bound_handle,
};
pub(crate) use constraints::{
    check_all_edge_constraints, check_all_node_constraints, check_node_property_constraint,
};
pub(crate) use context::{cached_regex, inc_stat};
pub(crate) use describe::{
    plan_has_projection, plan_is_mutating, plan_is_schema, plan_visible_columns, projection_name,
};
pub(crate) use shortest::{
    bfs_all_shortest, bfs_k_groups, bfs_k_shortest, bfs_single_shortest, dijkstra_single_shortest,
};

pub(crate) use crate::vex::expr::scalar::{compare_values, proc_values_equal};
pub(crate) use crate::vex::ops::aggregate::collect_graph_parts;
pub(crate) use crate::vex::values::{VIRTUAL_ID_BASE, cypher_value_to_property_map};
