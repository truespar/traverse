//! Eagerness analysis: where a streaming write must become a barrier.
//!
//! Cypher is defined clause by clause: each clause conceptually finishes over
//! every row before the next starts. The executor pulls one row at a time and
//! writes stream too (`CREATE`, `SET`, `REMOVE`, `MERGE` apply as each row
//! passes), so a read further down the pipeline could see a write that,
//! clause by clause, hasn't happened yet:
//!
//! ```cypher
//! MATCH (n:P) CREATE (:P)              -- the scan must not see the new nodes
//! MATCH (a)-[:R]->(b) CREATE (a)-[:R]->(b)   -- the expand must not see the new edges
//! UNWIND [1,2] AS x MERGE (a:P {id: x}) CREATE (:P {id: x + 1})
//!                                      -- row 2's MERGE must not find row 1's CREATE
//! ```
//!
//! This pass walks the plan bottom-up, tracking per subtree which reads and
//! writes are still *in flight* when a row leaves it (reads below a
//! materializing operator, such as a sort, aggregation or join build side, are
//! done). Where a write's effects overlap an in-flight read, or a read overlaps
//! an in-flight write, an [`Eager`](LogicalPlan::Eager) operator goes between
//! them: it drains its input (charged to the query memory budget) before
//! emitting, restoring clause-at-a-time order exactly where it's observable.
//! Same model as Neo4j's planner, shown the same way in `EXPLAIN`.
//!
//! Conservative by construction: anything unclassifiable (procedure calls,
//! dynamic labels, `SET n = $map`) reads or writes "everything" and forces a
//! barrier. The common import shapes (`UNWIND $rows ... CREATE`,
//! `LOAD CSV ... MERGE`, `MATCH (a {key}) MATCH (b {key}) CREATE (a)-[:R]->(b)`,
//! `MATCH (n:L) SET n.flag = true`) stream with no buffering.

mod analyze;
mod reads;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::planner::LogicalPlan;

use analyze::analyze;

// -- Entity kinds and variable context ---------------------------------

/// What a variable is bound to, as far as the plan says.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// Node; `labels` it's known to carry (may be empty).
    Node(Vec<String>),
    /// Relationship; `types` it may have (empty = any).
    Rel(Vec<String>),
    /// Plain value (list, map, scalar): property access on it isn't a graph read.
    Value,
    /// Anything, including entities smuggled through a list or function.
    Unknown,
}

/// Variable -> kind, built bottom-up.
#[derive(Default, Clone)]
struct VarKinds {
    map: FxHashMap<String, Kind>,
}

impl VarKinds {
    fn bind(&mut self, var: &str, kind: Kind) {
        self.map.insert(var.to_string(), kind);
    }

    /// Binds only if unbound (an already-bound expand target keeps its labels).
    fn bind_if_new(&mut self, var: &str, kind: Kind) {
        self.map.entry(var.to_string()).or_insert(kind);
    }

    fn add_label(&mut self, var: &str, label: &str) {
        match self.map.get_mut(var) {
            Some(Kind::Node(labels)) => {
                if !labels.iter().any(|l| l == label) {
                    labels.push(label.to_string());
                }
            }
            Some(Kind::Unknown) | None => {
                self.map
                    .insert(var.to_string(), Kind::Node(vec![label.to_string()]));
            }
            _ => {}
        }
    }

    fn kind(&self, var: &str) -> Kind {
        self.map.get(var).cloned().unwrap_or(Kind::Unknown)
    }
}

// -- Read and write sets -----------------------------------------------

/// Entity class of a property access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropKind {
    Node,
    Rel,
    Any,
}

impl PropKind {
    fn of(kind: &Kind) -> Option<PropKind> {
        match kind {
            Kind::Node(_) => Some(PropKind::Node),
            Kind::Rel(_) => Some(PropKind::Rel),
            Kind::Value => None,
            Kind::Unknown => Some(PropKind::Any),
        }
    }

    fn overlaps(self, other: PropKind) -> bool {
        self == PropKind::Any || other == PropKind::Any || self == other
    }
}

/// One property key read or written, with what's known about the entity.
#[derive(Clone, Debug)]
struct PropRef {
    key: String,
    kind: PropKind,
    /// Labels the entity is known to carry; empty = unknown/any. Exact for *created* nodes, so a
    /// read on `P` can't see a node created as `{Q}`.
    labels: Vec<String>,
}

impl PropRef {
    fn overlaps(&self, other: &PropRef) -> bool {
        self.key == other.key
            && self.kind.overlaps(other.kind)
            && (self.labels.is_empty()
                || other.labels.is_empty()
                || self.labels.iter().any(|l| other.labels.contains(l)))
    }
}

/// Graph reads a subtree may still be doing when rows leave it.
#[derive(Default, Clone, Debug)]
struct Reads {
    /// Node lookups by label (scans, index seeks, label predicates).
    node_labels: FxHashSet<String>,
    /// Unlabeled node scan: sees every node.
    all_nodes: bool,
    /// Relationship traversals by type.
    rel_types: FxHashSet<String>,
    /// Untyped traversal: sees every relationship.
    all_rels: bool,
    /// `labels(n)`: observes label changes on existing nodes.
    label_values: bool,
    props: Vec<PropRef>,
    /// `properties(n)`, `keys(n)`, `n{.*}`: every property of that kind.
    all_props: Option<PropKind>,
    /// Procedure calls and other opaque readers.
    everything: bool,
}

impl Reads {
    fn is_empty(&self) -> bool {
        !self.everything
            && !self.all_nodes
            && !self.all_rels
            && !self.label_values
            && self.all_props.is_none()
            && self.node_labels.is_empty()
            && self.rel_types.is_empty()
            && self.props.is_empty()
    }

    fn merge(&mut self, other: &Reads) {
        self.node_labels.extend(other.node_labels.iter().cloned());
        self.all_nodes |= other.all_nodes;
        self.rel_types.extend(other.rel_types.iter().cloned());
        self.all_rels |= other.all_rels;
        self.label_values |= other.label_values;
        self.props.extend(other.props.iter().cloned());
        self.all_props = merge_prop_kind(self.all_props, other.all_props);
        self.everything |= other.everything;
    }

    fn read_prop(&mut self, key: &str, kind: &Kind) {
        if let Some(pk) = PropKind::of(kind) {
            let labels = match kind {
                Kind::Node(labels) => labels.clone(),
                _ => Vec::new(),
            };
            self.props.push(PropRef {
                key: key.to_string(),
                kind: pk,
                labels,
            });
        }
    }

    fn read_all_props(&mut self, kind: &Kind) {
        if let Some(pk) = PropKind::of(kind) {
            self.all_props = merge_prop_kind(self.all_props, Some(pk));
        }
    }

    fn read_rel_types(&mut self, types: &[String]) {
        if types.is_empty() {
            self.all_rels = true;
        } else {
            self.rel_types.extend(types.iter().cloned());
        }
    }
}

fn merge_prop_kind(a: Option<PropKind>, b: Option<PropKind>) -> Option<PropKind> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(x), Some(y)) if x == y => Some(x),
        _ => Some(PropKind::Any),
    }
}

/// Graph writes a subtree may still be applying when rows leave it.
#[derive(Default, Clone, Debug)]
struct Writes {
    /// Label sets of created nodes (empty = unlabeled node).
    created_nodes: Vec<Vec<String>>,
    /// A node created with labels computed at run time.
    created_any_label: bool,
    /// Labels added to / removed from existing nodes.
    changed_labels: FxHashSet<String>,
    changed_any_label: bool,
    created_rel_types: FxHashSet<String>,
    created_any_rel_type: bool,
    props: Vec<PropRef>,
    /// `SET n = {...}`, `SET n += {...}`, dynamic keys: unknown property keys.
    all_props: Option<PropKind>,
    deletes: bool,
    /// Opaque writers (procedures).
    everything: bool,
}

impl Writes {
    fn is_empty(&self) -> bool {
        !self.everything
            && !self.deletes
            && !self.created_any_label
            && !self.changed_any_label
            && !self.created_any_rel_type
            && self.all_props.is_none()
            && self.created_nodes.is_empty()
            && self.changed_labels.is_empty()
            && self.created_rel_types.is_empty()
            && self.props.is_empty()
    }

    fn merge(&mut self, other: &Writes) {
        self.created_nodes
            .extend(other.created_nodes.iter().cloned());
        self.created_any_label |= other.created_any_label;
        self.changed_labels
            .extend(other.changed_labels.iter().cloned());
        self.changed_any_label |= other.changed_any_label;
        self.created_rel_types
            .extend(other.created_rel_types.iter().cloned());
        self.created_any_rel_type |= other.created_any_rel_type;
        self.props.extend(other.props.iter().cloned());
        self.all_props = merge_prop_kind(self.all_props, other.all_props);
        self.deletes |= other.deletes;
        self.everything |= other.everything;
    }

    fn write_prop(&mut self, key: &str, kind: PropKind, labels: Vec<String>) {
        self.props.push(PropRef {
            key: key.to_string(),
            kind,
            labels,
        });
    }

    fn write_all_props(&mut self, kind: PropKind) {
        self.all_props = merge_prop_kind(self.all_props, Some(kind));
    }
}

/// Could a write in `w`, applied while the reads in `r` run, change what they return?
fn conflicts(r: &Reads, w: &Writes) -> bool {
    if r.is_empty() || w.is_empty() {
        return false;
    }
    if r.everything || w.everything || w.deletes {
        return true;
    }
    // created nodes show up in scans of their labels (or all nodes); existing relationships can't
    // reach them
    if w.created_any_label && (r.all_nodes || !r.node_labels.is_empty()) {
        return true;
    }
    for labels in &w.created_nodes {
        if r.all_nodes || labels.iter().any(|l| r.node_labels.contains(l)) {
            return true;
        }
    }
    // label changes move existing nodes in and out of label scans and change `labels(n)`
    if (w.changed_any_label || !w.changed_labels.is_empty()) && r.label_values {
        return true;
    }
    if w.changed_any_label && !r.node_labels.is_empty() {
        return true;
    }
    if w.changed_labels.iter().any(|l| r.node_labels.contains(l)) {
        return true;
    }
    // created relationships show up in traversals of their type
    if (w.created_any_rel_type || !w.created_rel_types.is_empty()) && r.all_rels {
        return true;
    }
    if w.created_any_rel_type && !r.rel_types.is_empty() {
        return true;
    }
    if w.created_rel_types.iter().any(|t| r.rel_types.contains(t)) {
        return true;
    }
    // property writes vs property reads of a compatible entity
    if let Some(rk) = r.all_props
        && (w.all_props.is_some_and(|wk| wk.overlaps(rk))
            || w.props.iter().any(|p| p.kind.overlaps(rk)))
    {
        return true;
    }
    if let Some(wk) = w.all_props
        && r.props.iter().any(|p| p.kind.overlaps(wk))
    {
        return true;
    }
    w.props
        .iter()
        .any(|wp| r.props.iter().any(|rp| rp.overlaps(wp)))
}

fn eager(input: Box<LogicalPlan>) -> Box<LogicalPlan> {
    if matches!(*input, LogicalPlan::Eager { .. }) {
        return input;
    }
    Box::new(LogicalPlan::Eager { input })
}

/// Inserts `Eager` barriers wherever a streaming write could be seen out of clause order. Read-only
/// plans come back unchanged.
pub fn insert_eager_barriers(plan: LogicalPlan) -> LogicalPlan {
    if !super::is_mutating_plan(&plan) {
        return plan;
    }
    let mut ctx = VarKinds::default();
    analyze(plan, &mut ctx).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(labels: &[&str]) -> Kind {
        Kind::Node(labels.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn created_label_conflicts_with_scan_of_that_label_only() {
        let mut r = Reads::default();
        r.node_labels.insert("P".into());
        let mut w = Writes::default();
        w.created_nodes.push(vec!["Q".into()]);
        assert!(!conflicts(&r, &w));
        w.created_nodes.push(vec!["P".into()]);
        assert!(conflicts(&r, &w));
    }

    #[test]
    fn unlabeled_scan_sees_every_created_node() {
        let r = Reads {
            all_nodes: true,
            ..Default::default()
        };
        let mut w = Writes::default();
        w.created_nodes.push(Vec::new());
        assert!(conflicts(&r, &w));
    }

    #[test]
    fn property_conflicts_respect_label_context() {
        let mut r = Reads::default();
        r.read_prop("id", &node(&["P"]));
        let mut w = Writes::default();
        w.write_prop("id", PropKind::Node, vec!["Q".into()]);
        assert!(
            !conflicts(&r, &w),
            "a created Q node cannot satisfy a read on P"
        );
        w.write_prop("id", PropKind::Node, Vec::new());
        assert!(conflicts(&r, &w), "an unknown-label write might");
    }

    #[test]
    fn value_property_access_is_not_a_graph_read() {
        let mut r = Reads::default();
        r.read_prop("id", &Kind::Value);
        let mut w = Writes::default();
        w.write_prop("id", PropKind::Node, Vec::new());
        assert!(!conflicts(&r, &w));
    }

    #[test]
    fn relationship_types_and_deletes() {
        let mut r = Reads::default();
        r.rel_types.insert("R".into());
        let mut w = Writes::default();
        w.created_rel_types.insert("S".into());
        assert!(!conflicts(&r, &w));
        w.created_rel_types.insert("R".into());
        assert!(conflicts(&r, &w));
        let d = Writes {
            deletes: true,
            ..Default::default()
        };
        let mut r2 = Reads::default();
        r2.node_labels.insert("X".into());
        assert!(conflicts(&r2, &d));
    }
}
