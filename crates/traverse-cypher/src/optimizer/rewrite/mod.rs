//! Rule-based rewrite passes of the query optimizer: constant folding, access
//! paths (index / id / edge-type / text / spatial seeks, lookup joins),
//! conjunction splitting, filter push-down and fusion, hash joins, and fused
//! operators (DistinctExpand, ScanFilterProject).

mod carry;
mod expand;
mod filter;
mod fold;
mod fuse;
mod index;
mod join;
mod seek;
#[cfg(test)]
mod tests;
mod walk;

pub use carry::lift_scalar_carry_aggregate;
pub use expand::reverse_expand_direction;
pub use filter::{
    fuse_filters, push_filters_down, split_conjunctions, split_or_filter_into_union_seeks,
};
pub use fold::fold_constants;
pub use fuse::{fuse_distinct_expand, fuse_scan_filter_project};
pub use index::{
    convert_filter_to_index_scan, convert_spatial_predicates, convert_text_predicates,
};
pub use join::{convert_cross_product_to_index_lookup_join, convert_cross_to_hash_join};
pub use seek::{
    convert_anchorless_expand_to_edge_type_scan, convert_filter_to_node_by_id,
    convert_filter_to_node_by_id_range, push_label_filters_into_scan,
};
pub use walk::{plan_output_variables, recurse_transform};
