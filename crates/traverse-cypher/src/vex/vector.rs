//! Typed vectors: one column of a chunk.
//!
//! A [`Vector`] holds up to [`VECTOR_CAPACITY`] lanes of one physical type with a validity mask.
//! Property gathers build the narrowest variant that holds every non-null lane (integers stay
//! `Int`, strings land in one contiguous buffer) and fall back to [`Vector::Any`] for mixed or
//! structured values. Kernels dispatch on the variant per chunk; [`ValueRef`] is the lane view
//! every kernel can fall back to.

use traverse_core::graph::batch::ColumnSink;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeId, EdgeTypeId, NodeId};

use crate::types::CypherValue;

/// Lanes a vector holds at most; sources fill up to this many per chunk.
pub const VECTOR_CAPACITY: usize = 2048;

/// One column of lanes; see the module docs.
#[derive(Debug, Clone)]
pub enum Vector {
    /// 64-bit integers.
    Int {
        /// Values (garbage where invalid).
        vals: Vec<i64>,
        /// Validity per lane.
        valid: Vec<bool>,
    },
    /// 64-bit floats.
    Float {
        /// Values.
        vals: Vec<f64>,
        /// Validity per lane.
        valid: Vec<bool>,
    },
    /// Booleans.
    Bool {
        /// Values.
        vals: Vec<bool>,
        /// Validity per lane.
        valid: Vec<bool>,
    },
    /// Node references.
    Node {
        /// Ids.
        ids: Vec<NodeId>,
        /// Validity per lane.
        valid: Vec<bool>,
    },
    /// Relationship references with their topology.
    Edge {
        /// Ids.
        ids: Vec<EdgeId>,
        /// Types.
        types: Vec<EdgeTypeId>,
        /// Start nodes.
        srcs: Vec<NodeId>,
        /// End nodes.
        tgts: Vec<NodeId>,
        /// Validity per lane.
        valid: Vec<bool>,
    },
    /// Strings in one buffer: lane `i` is `data[offsets[i]..offsets[i + 1]]`.
    Str {
        /// Lane boundaries, `len + 1` entries.
        offsets: Vec<u32>,
        /// UTF-8 bytes.
        data: Vec<u8>,
        /// Validity per lane.
        valid: Vec<bool>,
    },
    /// Anything else, or mixed lanes; `Null` lanes are the invalid ones.
    Any {
        /// Values.
        vals: Vec<CypherValue>,
    },
}

/// A borrowed lane. `Any` only carries values that have no typed form (lists, maps, paths,
/// temporals, bytes, materialized entities); scalars arrive in their typed variants whichever
/// vector they came from.
#[derive(Debug, Clone, Copy)]
pub enum ValueRef<'a> {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// Node reference.
    Node(NodeId),
    /// Relationship reference.
    Edge {
        /// Id.
        id: EdgeId,
        /// Type.
        etype: EdgeTypeId,
        /// Start node.
        src: NodeId,
        /// End node.
        tgt: NodeId,
    },
    /// String.
    Str(&'a str),
    /// A value with no typed form.
    Any(&'a CypherValue),
}

impl<'a> ValueRef<'a> {
    /// Is it null?
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, ValueRef::Null)
    }

    /// The lane as an owned value.
    pub fn to_owned(&self) -> CypherValue {
        match *self {
            ValueRef::Null => CypherValue::Null,
            ValueRef::Bool(b) => CypherValue::Bool(b),
            ValueRef::Int(i) => CypherValue::Integer(i),
            ValueRef::Float(f) => CypherValue::Float(f),
            ValueRef::Node(id) => CypherValue::NodeRef(id),
            ValueRef::Edge {
                id,
                etype,
                src,
                tgt,
            } => CypherValue::EdgeRef {
                id,
                edge_type_id: etype,
                source: src,
                target: tgt,
            },
            ValueRef::Str(s) => CypherValue::String(s.to_string()),
            ValueRef::Any(v) => v.clone(),
        }
    }

    /// A value's lane view: scalars typed, the rest as `Any`.
    #[inline]
    pub fn of(v: &'a CypherValue) -> ValueRef<'a> {
        match v {
            CypherValue::Null => ValueRef::Null,
            CypherValue::Bool(b) => ValueRef::Bool(*b),
            CypherValue::Integer(i) => ValueRef::Int(*i),
            CypherValue::Float(f) => ValueRef::Float(*f),
            CypherValue::NodeRef(id) => ValueRef::Node(*id),
            CypherValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => ValueRef::Edge {
                id: *id,
                etype: *edge_type_id,
                src: *source,
                tgt: *target,
            },
            CypherValue::String(s) => ValueRef::Str(s),
            other => ValueRef::Any(other),
        }
    }

    /// The node id of a node lane (a reference or a materialized node).
    #[inline]
    pub fn node_id(&self) -> Option<NodeId> {
        match self {
            ValueRef::Node(id) => Some(*id),
            ValueRef::Any(CypherValue::Node { id, .. }) => Some(*id),
            _ => None,
        }
    }

    /// The edge id of a relationship lane.
    #[inline]
    pub fn edge_id(&self) -> Option<EdgeId> {
        match self {
            ValueRef::Edge { id, .. } => Some(*id),
            ValueRef::Any(CypherValue::Edge { id, .. }) => Some(*id),
            _ => None,
        }
    }

    /// Type name for error messages, as [`CypherValue::type_name`] gives it.
    pub fn type_name(&self) -> &'static str {
        match self {
            ValueRef::Null => "NULL",
            ValueRef::Bool(_) => "BOOLEAN",
            ValueRef::Int(_) => "INTEGER",
            ValueRef::Float(_) => "FLOAT",
            ValueRef::Node(_) => "NODE",
            ValueRef::Edge { .. } => "RELATIONSHIP",
            ValueRef::Str(_) => "STRING",
            ValueRef::Any(v) => v.type_name(),
        }
    }
}

impl Vector {
    /// An empty vector of the `Any` variant.
    pub fn empty() -> Self {
        Vector::Any { vals: Vec::new() }
    }

    /// `n` null lanes.
    pub fn nulls(n: usize) -> Self {
        Vector::Any {
            vals: vec![CypherValue::Null; n],
        }
    }

    /// An `Int` vector of valid values.
    pub fn ints(vals: Vec<i64>) -> Self {
        let valid = vec![true; vals.len()];
        Vector::Int { vals, valid }
    }

    /// A `Node` vector of valid ids.
    pub fn nodes(ids: Vec<NodeId>) -> Self {
        let valid = vec![true; ids.len()];
        Vector::Node { ids, valid }
    }

    /// A `Bool` vector of valid values.
    pub fn bools(vals: Vec<bool>) -> Self {
        let valid = vec![true; vals.len()];
        Vector::Bool { vals, valid }
    }

    /// A vector from owned values, narrowed to a typed variant when they allow it.
    pub fn from_values(vals: Vec<CypherValue>) -> Self {
        let mut b = VectorBuilder::with_capacity(vals.len());
        for v in vals {
            b.push_owned(v);
        }
        b.finish()
    }

    /// Lanes.
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Vector::Int { vals, .. } => vals.len(),
            Vector::Float { vals, .. } => vals.len(),
            Vector::Bool { vals, .. } => vals.len(),
            Vector::Node { ids, .. } => ids.len(),
            Vector::Edge { ids, .. } => ids.len(),
            Vector::Str { valid, .. } => valid.len(),
            Vector::Any { vals } => vals.len(),
        }
    }

    /// No lanes?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Is lane `i` null?
    #[inline]
    pub fn is_null(&self, i: usize) -> bool {
        match self {
            Vector::Int { valid, .. }
            | Vector::Float { valid, .. }
            | Vector::Bool { valid, .. }
            | Vector::Node { valid, .. }
            | Vector::Edge { valid, .. }
            | Vector::Str { valid, .. } => !valid[i],
            Vector::Any { vals } => matches!(vals[i], CypherValue::Null),
        }
    }

    /// Validity per lane for the typed variants; `None` for `Any` (whose nulls are explicit).
    #[inline]
    pub fn validity(&self) -> Option<&[bool]> {
        match self {
            Vector::Int { valid, .. }
            | Vector::Float { valid, .. }
            | Vector::Bool { valid, .. }
            | Vector::Node { valid, .. }
            | Vector::Edge { valid, .. }
            | Vector::Str { valid, .. } => Some(valid),
            Vector::Any { .. } => None,
        }
    }

    /// Lanes holding a value: of `sel`, or all of them.
    pub fn non_null_count(&self, sel: Option<&[u32]>) -> usize {
        match (self.validity(), sel) {
            (Some(valid), Some(sel)) => sel.iter().filter(|&&l| valid[l as usize]).count(),
            (Some(valid), None) => valid.iter().filter(|ok| **ok).count(),
            (None, sel) => {
                let Vector::Any { vals } = self else {
                    unreachable!("only Any lacks validity bits");
                };
                match sel {
                    Some(sel) => sel.iter().filter(|&&l| !vals[l as usize].is_null()).count(),
                    None => vals.iter().filter(|v| !v.is_null()).count(),
                }
            }
        }
    }

    /// Lane `i`, borrowed.
    #[inline]
    pub fn get(&self, i: usize) -> ValueRef<'_> {
        match self {
            Vector::Int { vals, valid } => {
                if valid[i] {
                    ValueRef::Int(vals[i])
                } else {
                    ValueRef::Null
                }
            }
            Vector::Float { vals, valid } => {
                if valid[i] {
                    ValueRef::Float(vals[i])
                } else {
                    ValueRef::Null
                }
            }
            Vector::Bool { vals, valid } => {
                if valid[i] {
                    ValueRef::Bool(vals[i])
                } else {
                    ValueRef::Null
                }
            }
            Vector::Node { ids, valid } => {
                if valid[i] {
                    ValueRef::Node(ids[i])
                } else {
                    ValueRef::Null
                }
            }
            Vector::Edge {
                ids,
                types,
                srcs,
                tgts,
                valid,
            } => {
                if valid[i] {
                    ValueRef::Edge {
                        id: ids[i],
                        etype: types[i],
                        src: srcs[i],
                        tgt: tgts[i],
                    }
                } else {
                    ValueRef::Null
                }
            }
            Vector::Str {
                offsets,
                data,
                valid,
            } => {
                if valid[i] {
                    let bytes = &data[offsets[i] as usize..offsets[i + 1] as usize];
                    // SAFETY: every string lane was pushed from a `&str`, so the bytes are UTF-8
                    ValueRef::Str(unsafe { std::str::from_utf8_unchecked(bytes) })
                } else {
                    ValueRef::Null
                }
            }
            Vector::Any { vals } => ValueRef::of(&vals[i]),
        }
    }

    /// Lane `i`, owned.
    #[inline]
    pub fn value(&self, i: usize) -> CypherValue {
        match self {
            Vector::Any { vals } => vals[i].clone(),
            other => other.get(i).to_owned(),
        }
    }

    /// The node id of lane `i` if it holds a node.
    #[inline]
    pub fn node_id(&self, i: usize) -> Option<NodeId> {
        match self {
            Vector::Node { ids, valid } => valid[i].then_some(ids[i]),
            Vector::Any { vals } => match &vals[i] {
                CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => Some(*id),
                _ => None,
            },
            _ => None,
        }
    }

    /// A new vector of the lanes in `sel`, in that order.
    pub fn take(&self, sel: &[u32]) -> Vector {
        match self {
            Vector::Int { vals, valid } => Vector::Int {
                vals: sel.iter().map(|&i| vals[i as usize]).collect(),
                valid: sel.iter().map(|&i| valid[i as usize]).collect(),
            },
            Vector::Float { vals, valid } => Vector::Float {
                vals: sel.iter().map(|&i| vals[i as usize]).collect(),
                valid: sel.iter().map(|&i| valid[i as usize]).collect(),
            },
            Vector::Bool { vals, valid } => Vector::Bool {
                vals: sel.iter().map(|&i| vals[i as usize]).collect(),
                valid: sel.iter().map(|&i| valid[i as usize]).collect(),
            },
            Vector::Node { ids, valid } => Vector::Node {
                ids: sel.iter().map(|&i| ids[i as usize]).collect(),
                valid: sel.iter().map(|&i| valid[i as usize]).collect(),
            },
            Vector::Edge {
                ids,
                types,
                srcs,
                tgts,
                valid,
            } => Vector::Edge {
                ids: sel.iter().map(|&i| ids[i as usize]).collect(),
                types: sel.iter().map(|&i| types[i as usize]).collect(),
                srcs: sel.iter().map(|&i| srcs[i as usize]).collect(),
                tgts: sel.iter().map(|&i| tgts[i as usize]).collect(),
                valid: sel.iter().map(|&i| valid[i as usize]).collect(),
            },
            Vector::Str { .. } => {
                let mut b = VectorBuilder::with_capacity(sel.len());
                for &i in sel {
                    b.push(self.get(i as usize));
                }
                b.finish()
            }
            Vector::Any { vals } => Vector::Any {
                vals: sel.iter().map(|&i| vals[i as usize].clone()).collect(),
            },
        }
    }

    /// Lanes `[start, end)` as a new vector.
    pub fn slice(&self, start: usize, end: usize) -> Vector {
        match self {
            Vector::Int { vals, valid } => Vector::Int {
                vals: vals[start..end].to_vec(),
                valid: valid[start..end].to_vec(),
            },
            Vector::Float { vals, valid } => Vector::Float {
                vals: vals[start..end].to_vec(),
                valid: valid[start..end].to_vec(),
            },
            Vector::Bool { vals, valid } => Vector::Bool {
                vals: vals[start..end].to_vec(),
                valid: valid[start..end].to_vec(),
            },
            Vector::Node { ids, valid } => Vector::Node {
                ids: ids[start..end].to_vec(),
                valid: valid[start..end].to_vec(),
            },
            Vector::Edge {
                ids,
                types,
                srcs,
                tgts,
                valid,
            } => Vector::Edge {
                ids: ids[start..end].to_vec(),
                types: types[start..end].to_vec(),
                srcs: srcs[start..end].to_vec(),
                tgts: tgts[start..end].to_vec(),
                valid: valid[start..end].to_vec(),
            },
            Vector::Str {
                offsets,
                data,
                valid,
            } => {
                let base = offsets[start];
                Vector::Str {
                    offsets: offsets[start..=end].iter().map(|o| o - base).collect(),
                    data: data[base as usize..offsets[end] as usize].to_vec(),
                    valid: valid[start..end].to_vec(),
                }
            }
            Vector::Any { vals } => Vector::Any {
                vals: vals[start..end].to_vec(),
            },
        }
    }

    /// One vector of `parts` end to end; parts of one typed variant stay typed.
    pub fn concat(parts: Vec<Vector>) -> Vector {
        if parts.len() == 1 {
            return parts.into_iter().next().unwrap();
        }
        let n: usize = parts.iter().map(Vector::len).sum();
        let same = |f: fn(&Vector) -> bool| parts.iter().all(f);
        if same(|v| matches!(v, Vector::Int { .. })) {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for p in parts {
                if let Vector::Int { vals: v, valid: ok } = p {
                    vals.extend(v);
                    valid.extend(ok);
                }
            }
            return Vector::Int { vals, valid };
        }
        if same(|v| matches!(v, Vector::Float { .. })) {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for p in parts {
                if let Vector::Float { vals: v, valid: ok } = p {
                    vals.extend(v);
                    valid.extend(ok);
                }
            }
            return Vector::Float { vals, valid };
        }
        if same(|v| matches!(v, Vector::Node { .. })) {
            let mut ids = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for p in parts {
                if let Vector::Node { ids: v, valid: ok } = p {
                    ids.extend(v);
                    valid.extend(ok);
                }
            }
            return Vector::Node { ids, valid };
        }
        if same(|v| matches!(v, Vector::Any { .. })) {
            let mut vals = Vec::with_capacity(n);
            for p in parts {
                if let Vector::Any { vals: v } = p {
                    vals.extend(v);
                }
            }
            return Vector::Any { vals };
        }
        let mut b = VectorBuilder::with_capacity(n);
        for p in &parts {
            for i in 0..p.len() {
                b.push(p.get(i));
            }
        }
        b.finish()
    }

    /// A new vector with lane `i` repeated `n` times.
    pub fn repeat(&self, i: usize, n: usize) -> Vector {
        let sel: Vec<u32> = std::iter::repeat_n(i as u32, n).collect();
        self.take(&sel)
    }

    /// Estimated heap bytes, for the query memory budget.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Vector::Int { vals, valid } => vals.len() * 8 + valid.len(),
            Vector::Float { vals, valid } => vals.len() * 8 + valid.len(),
            Vector::Bool { vals, valid } => vals.len() + valid.len(),
            Vector::Node { ids, valid } => ids.len() * 8 + valid.len(),
            Vector::Edge { ids, valid, .. } => ids.len() * 28 + valid.len(),
            Vector::Str { data, valid, .. } => data.len() + valid.len() * 5,
            Vector::Any { vals } => {
                vals.len() * std::mem::size_of::<CypherValue>()
                    + vals
                        .iter()
                        .map(crate::memory::value_heap_bytes)
                        .sum::<usize>()
            }
        }
    }

    /// Variant name, for EXPLAIN and errors.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Vector::Int { .. } => "INTEGER",
            Vector::Float { .. } => "FLOAT",
            Vector::Bool { .. } => "BOOLEAN",
            Vector::Node { .. } => "NODE",
            Vector::Edge { .. } => "RELATIONSHIP",
            Vector::Str { .. } => "STRING",
            Vector::Any { .. } => "ANY",
        }
    }

    /// The owned values of every lane.
    pub fn into_values(self) -> Vec<CypherValue> {
        match self {
            Vector::Any { vals } => vals,
            other => (0..other.len()).map(|i| other.value(i)).collect(),
        }
    }
}

/// Builds a vector lane by lane, starting in the narrowest variant the first non-null lane allows
/// and widening to `Any` on a type change. Implements [`ColumnSink`], so property gathers write
/// into it directly.
#[derive(Debug)]
pub struct VectorBuilder {
    state: BuildState,
    /// Lanes pushed before the first typed value, all null.
    leading_nulls: usize,
}

#[derive(Debug)]
enum BuildState {
    /// Only nulls so far.
    Untyped,
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Node(Vec<NodeId>, Vec<bool>),
    Edge(
        Vec<EdgeId>,
        Vec<EdgeTypeId>,
        Vec<NodeId>,
        Vec<NodeId>,
        Vec<bool>,
    ),
    Str(Vec<u32>, Vec<u8>, Vec<bool>),
    Any(Vec<CypherValue>),
}

impl Default for VectorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self {
            state: BuildState::Untyped,
            leading_nulls: 0,
        }
    }

    /// An empty builder expecting `cap` lanes.
    pub fn with_capacity(cap: usize) -> Self {
        let _ = cap;
        Self::new()
    }

    /// Lanes pushed so far.
    pub fn len(&self) -> usize {
        match &self.state {
            BuildState::Untyped => self.leading_nulls,
            BuildState::Int(_, v)
            | BuildState::Float(_, v)
            | BuildState::Bool(_, v)
            | BuildState::Node(_, v)
            | BuildState::Edge(_, _, _, _, v)
            | BuildState::Str(_, _, v) => v.len(),
            BuildState::Any(vals) => vals.len(),
        }
    }

    /// No lanes yet?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Converts the typed state to `Any`, keeping every lane.
    fn widen(&mut self) {
        let n = self.len();
        let vals: Vec<CypherValue> = match std::mem::replace(&mut self.state, BuildState::Untyped) {
            BuildState::Untyped => vec![CypherValue::Null; n],
            BuildState::Any(vals) => vals,
            BuildState::Int(vals, valid) => vals
                .into_iter()
                .zip(valid)
                .map(|(v, ok)| {
                    if ok {
                        CypherValue::Integer(v)
                    } else {
                        CypherValue::Null
                    }
                })
                .collect(),
            BuildState::Float(vals, valid) => vals
                .into_iter()
                .zip(valid)
                .map(|(v, ok)| {
                    if ok {
                        CypherValue::Float(v)
                    } else {
                        CypherValue::Null
                    }
                })
                .collect(),
            BuildState::Bool(vals, valid) => vals
                .into_iter()
                .zip(valid)
                .map(|(v, ok)| {
                    if ok {
                        CypherValue::Bool(v)
                    } else {
                        CypherValue::Null
                    }
                })
                .collect(),
            BuildState::Node(ids, valid) => ids
                .into_iter()
                .zip(valid)
                .map(|(v, ok)| {
                    if ok {
                        CypherValue::NodeRef(v)
                    } else {
                        CypherValue::Null
                    }
                })
                .collect(),
            BuildState::Edge(ids, types, srcs, tgts, valid) => (0..ids.len())
                .map(|i| {
                    if valid[i] {
                        CypherValue::EdgeRef {
                            id: ids[i],
                            edge_type_id: types[i],
                            source: srcs[i],
                            target: tgts[i],
                        }
                    } else {
                        CypherValue::Null
                    }
                })
                .collect(),
            BuildState::Str(offsets, data, valid) => (0..valid.len())
                .map(|i| {
                    if valid[i] {
                        let bytes = &data[offsets[i] as usize..offsets[i + 1] as usize];
                        CypherValue::String(String::from_utf8_lossy(bytes).into_owned())
                    } else {
                        CypherValue::Null
                    }
                })
                .collect(),
        };
        self.state = BuildState::Any(vals);
    }

    /// Starts a typed state after `self.leading_nulls` nulls.
    fn start<T: Clone>(n: usize, filler: T) -> (Vec<T>, Vec<bool>) {
        (vec![filler; n], vec![false; n])
    }

    /// Appends a borrowed lane.
    pub fn push(&mut self, v: ValueRef<'_>) {
        match v {
            ValueRef::Null => self.push_null(),
            ValueRef::Bool(b) => self.push_bool(b),
            ValueRef::Int(i) => self.push_int(i),
            ValueRef::Float(f) => self.push_float(f),
            ValueRef::Node(id) => self.push_node(id),
            ValueRef::Edge {
                id,
                etype,
                src,
                tgt,
            } => self.push_edge(id, etype, src, tgt),
            ValueRef::Str(s) => self.push_str(s),
            ValueRef::Any(cv) => self.push_any(cv.clone()),
        }
    }

    /// Appends an owned value.
    pub fn push_owned(&mut self, v: CypherValue) {
        match v {
            CypherValue::Null => self.push_null(),
            CypherValue::Bool(b) => self.push_bool(b),
            CypherValue::Integer(i) => self.push_int(i),
            CypherValue::Float(f) => self.push_float(f),
            CypherValue::NodeRef(id) => self.push_node(id),
            CypherValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => self.push_edge(id, edge_type_id, source, target),
            CypherValue::String(s) => self.push_str(&s),
            other => self.push_any(other),
        }
    }

    /// Appends a node reference.
    pub fn push_node(&mut self, id: NodeId) {
        match &mut self.state {
            BuildState::Node(ids, valid) => {
                ids.push(id);
                valid.push(true);
            }
            BuildState::Untyped => {
                let (mut ids, mut valid) = Self::start(self.leading_nulls, NodeId(0));
                ids.push(id);
                valid.push(true);
                self.state = BuildState::Node(ids, valid);
            }
            BuildState::Any(vals) => vals.push(CypherValue::NodeRef(id)),
            _ => {
                self.widen();
                self.push_node(id);
            }
        }
    }

    /// Appends a relationship reference.
    pub fn push_edge(&mut self, id: EdgeId, etype: EdgeTypeId, src: NodeId, tgt: NodeId) {
        match &mut self.state {
            BuildState::Edge(ids, types, srcs, tgts, valid) => {
                ids.push(id);
                types.push(etype);
                srcs.push(src);
                tgts.push(tgt);
                valid.push(true);
            }
            BuildState::Untyped => {
                let n = self.leading_nulls;
                let mut ids = vec![EdgeId(0); n];
                let mut types = vec![EdgeTypeId(0); n];
                let mut srcs = vec![NodeId(0); n];
                let mut tgts = vec![NodeId(0); n];
                let mut valid = vec![false; n];
                ids.push(id);
                types.push(etype);
                srcs.push(src);
                tgts.push(tgt);
                valid.push(true);
                self.state = BuildState::Edge(ids, types, srcs, tgts, valid);
            }
            BuildState::Any(vals) => vals.push(CypherValue::EdgeRef {
                id,
                edge_type_id: etype,
                source: src,
                target: tgt,
            }),
            _ => {
                self.widen();
                self.push_edge(id, etype, src, tgt);
            }
        }
    }

    /// Appends a value that has no typed form.
    pub fn push_any(&mut self, v: CypherValue) {
        match &mut self.state {
            BuildState::Any(vals) => vals.push(v),
            _ => {
                self.widen();
                self.push_any(v);
            }
        }
    }

    /// The finished vector.
    pub fn finish(self) -> Vector {
        match self.state {
            BuildState::Untyped => Vector::nulls(self.leading_nulls),
            BuildState::Int(vals, valid) => Vector::Int { vals, valid },
            BuildState::Float(vals, valid) => Vector::Float { vals, valid },
            BuildState::Bool(vals, valid) => Vector::Bool { vals, valid },
            BuildState::Node(ids, valid) => Vector::Node { ids, valid },
            BuildState::Edge(ids, types, srcs, tgts, valid) => Vector::Edge {
                ids,
                types,
                srcs,
                tgts,
                valid,
            },
            BuildState::Str(offsets, data, valid) => Vector::Str {
                offsets,
                data,
                valid,
            },
            BuildState::Any(vals) => Vector::Any { vals },
        }
    }
}

impl ColumnSink for VectorBuilder {
    fn push_null(&mut self) {
        match &mut self.state {
            BuildState::Untyped => self.leading_nulls += 1,
            BuildState::Int(vals, valid) => {
                vals.push(0);
                valid.push(false);
            }
            BuildState::Float(vals, valid) => {
                vals.push(0.0);
                valid.push(false);
            }
            BuildState::Bool(vals, valid) => {
                vals.push(false);
                valid.push(false);
            }
            BuildState::Node(ids, valid) => {
                ids.push(NodeId(0));
                valid.push(false);
            }
            BuildState::Edge(ids, types, srcs, tgts, valid) => {
                ids.push(EdgeId(0));
                types.push(EdgeTypeId(0));
                srcs.push(NodeId(0));
                tgts.push(NodeId(0));
                valid.push(false);
            }
            BuildState::Str(offsets, _, valid) => {
                let end = *offsets.last().unwrap_or(&0);
                offsets.push(end);
                valid.push(false);
            }
            BuildState::Any(vals) => vals.push(CypherValue::Null),
        }
    }

    fn push_bool(&mut self, v: bool) {
        match &mut self.state {
            BuildState::Bool(vals, valid) => {
                vals.push(v);
                valid.push(true);
            }
            BuildState::Untyped => {
                let (mut vals, mut valid) = Self::start(self.leading_nulls, false);
                vals.push(v);
                valid.push(true);
                self.state = BuildState::Bool(vals, valid);
            }
            BuildState::Any(vals) => vals.push(CypherValue::Bool(v)),
            _ => {
                self.widen();
                self.push_bool(v);
            }
        }
    }

    fn push_int(&mut self, v: i64) {
        match &mut self.state {
            BuildState::Int(vals, valid) => {
                vals.push(v);
                valid.push(true);
            }
            BuildState::Untyped => {
                let (mut vals, mut valid) = Self::start(self.leading_nulls, 0i64);
                vals.push(v);
                valid.push(true);
                self.state = BuildState::Int(vals, valid);
            }
            BuildState::Any(vals) => vals.push(CypherValue::Integer(v)),
            _ => {
                self.widen();
                self.push_int(v);
            }
        }
    }

    fn push_float(&mut self, v: f64) {
        match &mut self.state {
            BuildState::Float(vals, valid) => {
                vals.push(v);
                valid.push(true);
            }
            BuildState::Untyped => {
                let (mut vals, mut valid) = Self::start(self.leading_nulls, 0.0f64);
                vals.push(v);
                valid.push(true);
                self.state = BuildState::Float(vals, valid);
            }
            BuildState::Any(vals) => vals.push(CypherValue::Float(v)),
            _ => {
                self.widen();
                self.push_float(v);
            }
        }
    }

    fn push_ints(&mut self, vals: &[i64], valid: &[bool]) {
        match &mut self.state {
            BuildState::Int(v, ok) => {
                v.extend_from_slice(vals);
                ok.extend_from_slice(valid);
            }
            BuildState::Untyped => {
                let (mut v, mut ok) = Self::start(self.leading_nulls, 0i64);
                v.extend_from_slice(vals);
                ok.extend_from_slice(valid);
                self.state = BuildState::Int(v, ok);
            }
            _ => {
                for (v, ok) in vals.iter().zip(valid) {
                    if *ok {
                        self.push_int(*v);
                    } else {
                        self.push_null();
                    }
                }
            }
        }
    }

    fn push_floats(&mut self, vals: &[f64], valid: &[bool]) {
        match &mut self.state {
            BuildState::Float(v, ok) => {
                v.extend_from_slice(vals);
                ok.extend_from_slice(valid);
            }
            BuildState::Untyped => {
                let (mut v, mut ok) = Self::start(self.leading_nulls, 0.0f64);
                v.extend_from_slice(vals);
                ok.extend_from_slice(valid);
                self.state = BuildState::Float(v, ok);
            }
            _ => {
                for (v, ok) in vals.iter().zip(valid) {
                    if *ok {
                        self.push_float(*v);
                    } else {
                        self.push_null();
                    }
                }
            }
        }
    }

    fn push_str(&mut self, v: &str) {
        // temporals are stored as tagged strings
        if v.starts_with("__cypher_") {
            self.push_owned(CypherValue::from_property(&PropertyValue::String(
                v.to_string(),
            )));
            return;
        }
        match &mut self.state {
            BuildState::Str(offsets, data, valid) => {
                data.extend_from_slice(v.as_bytes());
                offsets.push(data.len() as u32);
                valid.push(true);
            }
            BuildState::Untyped => {
                let n = self.leading_nulls;
                let mut offsets = vec![0u32; n + 1];
                let mut valid = vec![false; n];
                let mut data = Vec::with_capacity(v.len() * 8);
                data.extend_from_slice(v.as_bytes());
                offsets.push(data.len() as u32);
                valid.push(true);
                self.state = BuildState::Str(offsets, data, valid);
            }
            BuildState::Any(vals) => vals.push(CypherValue::String(v.to_string())),
            _ => {
                self.widen();
                self.push_str(v);
            }
        }
    }

    fn push_value(&mut self, v: PropertyValue) {
        self.push_owned(CypherValue::from_property(&v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_narrows_and_widens() {
        let mut b = VectorBuilder::new();
        b.push_null();
        b.push_int(3);
        b.push_null();
        let v = b.finish();
        assert!(matches!(v, Vector::Int { .. }));
        assert_eq!(v.len(), 3);
        assert!(v.is_null(0));
        assert!(matches!(v.get(1), ValueRef::Int(3)));
        assert!(v.is_null(2));

        let mut b = VectorBuilder::new();
        b.push_int(1);
        b.push_str("x");
        let v = b.finish();
        assert!(matches!(v, Vector::Any { .. }));
        assert_eq!(v.value(0), CypherValue::Integer(1));
        assert_eq!(v.value(1), CypherValue::String("x".into()));
    }

    #[test]
    fn strings_share_one_buffer() {
        let mut b = VectorBuilder::new();
        b.push_str("ab");
        b.push_null();
        b.push_str("");
        b.push_str("cde");
        let v = b.finish();
        assert!(matches!(v.get(0), ValueRef::Str("ab")));
        assert!(v.is_null(1));
        assert!(matches!(v.get(2), ValueRef::Str("")));
        assert!(matches!(v.get(3), ValueRef::Str("cde")));
        let t = v.take(&[3, 0]);
        assert!(matches!(t.get(0), ValueRef::Str("cde")));
        assert!(matches!(t.get(1), ValueRef::Str("ab")));
    }
}
