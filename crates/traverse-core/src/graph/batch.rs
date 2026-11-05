//! Batch access types: the typed column sink property gathers decode into, and the
//! struct-of-arrays adjacency a batch expansion fills.
//!
//! A query engine reading a property for thousands of entities wants the values in a typed
//! column, not one `PropertyValue` per entity. [`ColumnSink`] is that column's write side: the
//! compact property store decodes each entry straight into it (an integer stays an integer, a
//! string is copied once), and only the structured types (lists, maps, points, ...) go through a
//! [`PropertyValue`]. Every gather pushes exactly one value per entity per key, nulls for missing
//! entities and keys, so a sink's length always equals the batch length.

use super::property::PropertyValue;
use super::types::{EdgeId, EdgeTypeId, NodeId};

/// Write side of a typed column; see the module docs.
pub trait ColumnSink {
    /// No value (missing entity, missing key, or a stored null).
    fn push_null(&mut self);
    /// A boolean.
    fn push_bool(&mut self, v: bool);
    /// An integer.
    fn push_int(&mut self, v: i64);
    /// A float.
    fn push_float(&mut self, v: f64);
    /// A string, borrowed from storage: the sink copies it once.
    fn push_str(&mut self, v: &str);
    /// Any other value (bytes, list, map, temporal, point, vector).
    fn push_value(&mut self, v: PropertyValue);

    /// Integer lanes at once: `vals[i]` where `valid[i]`, null elsewhere.
    fn push_ints(&mut self, vals: &[i64], valid: &[bool]) {
        for (v, ok) in vals.iter().zip(valid) {
            if *ok {
                self.push_int(*v);
            } else {
                self.push_null();
            }
        }
    }

    /// Float lanes at once: `vals[i]` where `valid[i]`, null elsewhere.
    fn push_floats(&mut self, vals: &[f64], valid: &[bool]) {
        for (v, ok) in vals.iter().zip(valid) {
            if *ok {
                self.push_float(*v);
            } else {
                self.push_null();
            }
        }
    }
}

/// A comparison on one property, for counting in storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnCmp {
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `=`
    Eq,
    /// `<>`
    Neq,
}

impl ColumnCmp {
    /// Does an ordering of (value, bound) satisfy the comparison?
    #[inline]
    pub fn holds(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            ColumnCmp::Lt => ord == Less,
            ColumnCmp::Lte => ord != Greater,
            ColumnCmp::Gt => ord == Greater,
            ColumnCmp::Gte => ord != Less,
            ColumnCmp::Eq => ord == Equal,
            ColumnCmp::Neq => ord != Equal,
        }
    }
}

/// The scalar a property is compared with.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnBound {
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(String),
}

/// The simplest sink: one owned value per lane.
impl ColumnSink for Vec<Option<PropertyValue>> {
    fn push_null(&mut self) {
        self.push(None);
    }
    fn push_bool(&mut self, v: bool) {
        self.push(Some(PropertyValue::Bool(v)));
    }
    fn push_int(&mut self, v: i64) {
        self.push(Some(PropertyValue::Int(v)));
    }
    fn push_float(&mut self, v: f64) {
        self.push(Some(PropertyValue::Float(v)));
    }
    fn push_str(&mut self, v: &str) {
        self.push(Some(PropertyValue::String(v.to_string())));
    }
    fn push_value(&mut self, v: PropertyValue) {
        self.push(Some(v));
    }
}

/// Pushes a stored value into a sink, scalars by value, the rest boxed.
pub fn push_property(sink: &mut dyn ColumnSink, v: PropertyValue) {
    match v {
        PropertyValue::Null => sink.push_null(),
        PropertyValue::Bool(b) => sink.push_bool(b),
        PropertyValue::Int(i) => sink.push_int(i),
        PropertyValue::Float(f) => sink.push_float(f),
        PropertyValue::String(s) => sink.push_str(&s),
        other => sink.push_value(other),
    }
}

/// Adjacency of a batch of source nodes, one entry per (source, edge), in source order. The
/// sources themselves are the slice passed to the gather; `src_lane` indexes it.
#[derive(Debug, Default, Clone)]
pub struct AdjBatch {
    /// Index of the source in the batch's source slice.
    pub src_lane: Vec<u32>,
    /// The edge.
    pub edge: Vec<EdgeId>,
    /// Its type.
    pub edge_type: Vec<EdgeTypeId>,
    /// The node at the other end.
    pub neighbor: Vec<NodeId>,
    /// `true`: the source is the edge's start node and the neighbor its end; `false`: the reverse.
    pub outgoing: Vec<bool>,
}

impl AdjBatch {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every entry, keeping the allocations.
    pub fn clear(&mut self) {
        self.src_lane.clear();
        self.edge.clear();
        self.edge_type.clear();
        self.neighbor.clear();
        self.outgoing.clear();
    }

    /// Entries.
    pub fn len(&self) -> usize {
        self.edge.len()
    }

    /// No entries?
    pub fn is_empty(&self) -> bool {
        self.edge.is_empty()
    }

    /// Appends one entry.
    #[inline]
    pub fn push(
        &mut self,
        src_lane: u32,
        edge: EdgeId,
        edge_type: EdgeTypeId,
        neighbor: NodeId,
        outgoing: bool,
    ) {
        self.src_lane.push(src_lane);
        self.edge.push(edge);
        self.edge_type.push(edge_type);
        self.neighbor.push(neighbor);
        self.outgoing.push(outgoing);
    }

    /// Start node of entry `i`, given the batch's sources.
    #[inline]
    pub fn source(&self, i: usize, srcs: &[NodeId]) -> NodeId {
        if self.outgoing[i] {
            srcs[self.src_lane[i] as usize]
        } else {
            self.neighbor[i]
        }
    }

    /// End node of entry `i`, given the batch's sources.
    #[inline]
    pub fn target(&self, i: usize, srcs: &[NodeId]) -> NodeId {
        if self.outgoing[i] {
            self.neighbor[i]
        } else {
            srcs[self.src_lane[i] as usize]
        }
    }
}
