//! Redo-log replay: applies recovered transactions onto a store.
//!
//! `Database::open` loads the `.tvdb` snapshot, then replays committed records
//! with `commit_lsn > recovery_lsn` (the tail the snapshot lacks).
//!
//! Contract:
//! * One `WriteTx` per recovered transaction, in commit order, so transaction
//!   boundaries and visibility match the original run.
//! * `set_skip_redo()` on each: replayed ops are never logged again.
//! * `CreateNode` / `CreateEdge` force the *recorded* IDs (`_with_id` APIs).
//!   Later ops (`CreateEdge{src,dst}`, `SetNodeProperties{node_id}`,
//!   `DeleteNode`, `AddLabel`, ...) reference those IDs, and auto-assignment would
//!   drift as soon as the snapshot's `next_node_id` and the tail's first id differ.
//! * `DeleteNode` is non-cascading: `delete_node_cascading` logs a `DeleteEdge`
//!   per edge before the `DeleteNode`, so incident edges are gone by then.
//!
//! Keep the match exhaustive over [`Op`] (no wildcard): a variant without a
//! replay arm would silently lose data on recovery.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::graph::constraint::ConstraintType;
use crate::graph::property::PropertyValue;
use crate::graph::types::{EdgeId, NodeId};
use crate::graph::vector::{HnswParams, Similarity};
use crate::storage::rlog::record::Op;
use crate::storage::rlog::recovery::ReplayTransaction;
use crate::tx::MvccStore;
use crate::tx::view::GraphView;

/// Components of a composite index's synthetic key (joined with `COMPOSITE_PROP_SEP`).
fn composite_parts(property: &str) -> Option<Vec<String>> {
    property
        .contains(crate::graph::COMPOSITE_PROP_SEP)
        .then(|| {
            property
                .split(crate::graph::COMPOSITE_PROP_SEP)
                .map(str::to_string)
                .collect()
        })
}

/// Replays recovered transactions onto `store` in order, each atomically in its
/// own `WriteTx`. An error stops replay and propagates; the caller should refuse
/// to open rather than serve a partial graph.
pub fn apply_transactions(store: &MvccStore, txns: &[ReplayTransaction]) -> Result<()> {
    for t in txns {
        apply_ops(store, &t.ops)?;
    }
    Ok(())
}

fn apply_ops(store: &MvccStore, ops: &[Op]) -> Result<()> {
    let mut tx = store.begin_write();
    tx.set_skip_redo();
    for op in ops {
        match op {
            Op::CreateNode {
                node_id,
                labels,
                properties,
            } => {
                let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                tx.create_node_with_id(NodeId(*node_id), &label_refs, props)?;
            }
            Op::DeleteNode { node_id } => {
                tx.delete_node(NodeId(*node_id))?;
            }
            Op::SetNodeProperties {
                node_id,
                properties,
            } => {
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                tx.update_node_properties(NodeId(*node_id), props)?;
            }
            Op::AddLabel { node_id, label } => {
                tx.add_label(NodeId(*node_id), label)?;
            }
            Op::RemoveLabel { node_id, label } => {
                tx.remove_label(NodeId(*node_id), label)?;
            }
            Op::CreateEdge {
                edge_id,
                src,
                dst,
                edge_type,
                properties,
            } => {
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                tx.create_edge_with_id(
                    EdgeId(*edge_id),
                    NodeId(*src),
                    NodeId(*dst),
                    edge_type,
                    props,
                )?;
            }
            Op::DeleteEdge { edge_id } => {
                tx.delete_edge(EdgeId(*edge_id))?;
            }
            Op::SetEdgeProperties {
                edge_id,
                properties,
            } => {
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                tx.update_edge_properties(EdgeId(*edge_id), props)?;
            }
            Op::CreateIndex {
                name,
                label,
                property,
            } => match composite_parts(property) {
                Some(parts) => tx.create_composite_index(name, label, &parts, false)?,
                None => tx.create_index(name, label, property)?,
            },
            Op::DropIndex { name } => {
                tx.drop_index(name)?;
            }
            Op::CreateConstraint {
                name,
                label,
                property,
                kind,
            } => {
                let composite: Option<Vec<String>> =
                    if property.contains(crate::graph::COMPOSITE_PROP_SEP) {
                        Some(
                            property
                                .split(crate::graph::COMPOSITE_PROP_SEP)
                                .map(str::to_string)
                                .collect(),
                        )
                    } else {
                        None
                    };
                match (decode_constraint_kind(*kind), composite) {
                    (Some((is_rel, ctype)), Some(props)) => {
                        tx.create_composite_constraint(name, label, &props, ctype, is_rel)?
                    }
                    (Some((true, ctype)), None) => {
                        tx.create_relationship_constraint(name, label, property, ctype)?
                    }
                    (Some((false, ctype)), None) => {
                        tx.create_constraint(name, label, property, ctype)?
                    }
                    (None, _) => {
                        return Err(crate::error::Error::Serialization(format!(
                            "rlog replay: unknown constraint kind {kind}"
                        )));
                    }
                }
            }
            Op::DropConstraint { name } => {
                tx.drop_constraint(name)?;
            }
            Op::CreateEdgeIndex {
                name,
                edge_type,
                property,
            } => match property.as_deref().and_then(composite_parts) {
                Some(parts) => tx.create_composite_index(name, edge_type, &parts, true)?,
                None => tx.create_edge_index(name, edge_type, property.as_deref())?,
            },
            Op::DropEdgeIndex { name } => {
                tx.drop_edge_index(name)?;
            }
            Op::CreateVectorIndex {
                name,
                label,
                property,
                dimensions,
                similarity,
                m,
                ef_construction,
                ef_search,
                seed,
                quantize,
            } => {
                let sim = Similarity::parse(similarity).ok_or_else(|| {
                    crate::error::Error::Serialization(format!(
                        "rlog replay: unknown similarity '{similarity}'"
                    ))
                })?;
                let params = HnswParams {
                    m: *m as usize,
                    ef_construction: *ef_construction as usize,
                    ef_search: *ef_search as usize,
                    seed: *seed,
                    quantize: *quantize,
                };
                tx.create_vector_index(name, label, property, *dimensions as usize, sim, params)?;
            }
            Op::DropVectorIndex { name } => {
                tx.drop_vector_index(name)?;
            }
            Op::CreateFulltextIndex {
                name,
                labels,
                properties,
                analyzer,
            } => {
                let az =
                    crate::graph::fulltext::Analyzer::from_name(analyzer).ok_or_else(|| {
                        crate::error::Error::Serialization(format!(
                            "rlog replay: unknown fulltext analyzer '{analyzer}'"
                        ))
                    })?;
                let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let prop_refs: Vec<&str> = properties.iter().map(String::as_str).collect();
                tx.create_fulltext_index(name, &label_refs, &prop_refs, az)?;
            }
            Op::DropFulltextIndex { name } => {
                tx.drop_fulltext_index(name)?;
            }
            Op::CreatePointIndex {
                name,
                label,
                property,
            } => {
                tx.create_point_index(name, label, property)?;
            }
            Op::DropPointIndex { name } => {
                tx.drop_point_index(name)?;
            }
            Op::CreateTextIndex {
                name,
                label,
                property,
            } => {
                tx.create_text_index(name, label, property)?;
            }
            Op::DropTextIndex { name } => {
                tx.drop_text_index(name)?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Constraint `kind` byte -> `(is_relationship, type)`. Node kinds 1..=21, relationship kinds +100
/// (`REL_CONSTRAINT_KIND_OFFSET`); `None` if unknown.
fn decode_constraint_kind(kind: u8) -> Option<(bool, ConstraintType)> {
    use crate::graph::constraint::PropertyTypeKind as K;
    let is_rel = kind >= 100;
    let base = if is_rel { kind - 100 } else { kind };
    let ct = match base {
        1 => ConstraintType::Unique,
        2 => ConstraintType::Exists,
        3 => ConstraintType::NodeKey,
        10 => ConstraintType::Type(K::String),
        11 => ConstraintType::Type(K::Integer),
        12 => ConstraintType::Type(K::Float),
        13 => ConstraintType::Type(K::Boolean),
        14 => ConstraintType::Type(K::Point),
        15 => ConstraintType::Type(K::Date),
        16 => ConstraintType::Type(K::LocalTime),
        17 => ConstraintType::Type(K::ZonedTime),
        18 => ConstraintType::Type(K::LocalDatetime),
        19 => ConstraintType::Type(K::ZonedDatetime),
        20 => ConstraintType::Type(K::Duration),
        21 => ConstraintType::Type(K::List),
        _ => return None,
    };
    Some((is_rel, ct))
}
