//! Query planner: AST -> logical plan.

mod clauses;
mod part;
mod pattern;
mod plan;
mod statement;
mod vars;

pub use clauses::is_aggregate_function;
pub(crate) use clauses::simple_expr_key;
pub(crate) use part::make_has_label_filter;
pub use pattern::plan_match_pattern_pub;
pub use plan::{LogicalPlan, SpatialQuery, TextPredicateKind};
pub use statement::{plan_single_query, plan_statement_with_hints};
pub(crate) use vars::{collect_expr_variables, is_indexable_value};

/// (label, property_key) pairs with an index.
pub type IndexHints = std::collections::HashSet<(String, String)>;
