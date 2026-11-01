// missing docs allowed for rkyv-generated resolver types here
#![allow(missing_docs)]

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Value of a node or edge property.
#[derive(
    Debug, Clone, PartialEq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize,
)]
#[rkyv(serialize_bounds(
    __S: rkyv::ser::Allocator + rkyv::ser::Writer,
    <__S as rkyv::rancor::Fallible>::Error: rkyv::rancor::Source,
))]
#[rkyv(deserialize_bounds(
    <__D as rkyv::rancor::Fallible>::Error: rkyv::rancor::Source,
))]
#[rkyv(bytecheck(bounds(
    __C: rkyv::validation::ArchiveContext,
    <__C as rkyv::rancor::Fallible>::Error: rkyv::rancor::Source,
)))]
pub enum PropertyValue {
    /// Null / no value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// Ordered list.
    List(#[rkyv(omit_bounds)] Vec<PropertyValue>),
    /// String-keyed map.
    Map(#[rkyv(omit_bounds)] BTreeMap<String, PropertyValue>),
    /// Date-time, ms since the Unix epoch.
    DateTime(i64),
    /// Duration, ms.
    Duration(i64),
    /// 2D spatial point.
    Point2D {
        /// X coordinate.
        x: f64,
        /// Y coordinate.
        y: f64,
        /// Spatial reference system id.
        srid: u32,
    },
    /// 3D spatial point.
    Point3D {
        /// X coordinate.
        x: f64,
        /// Y coordinate.
        y: f64,
        /// Z coordinate.
        z: f64,
        /// Spatial reference system id.
        srid: u32,
    },
    /// f32 vector (embeddings / similarity search).
    Vector(Vec<f32>),
}

impl PropertyValue {
    /// Is `Null`?
    pub fn is_null(&self) -> bool {
        matches!(self, PropertyValue::Null)
    }

    /// Bool value, if `Bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            PropertyValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// Integer value, if `Int`.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            PropertyValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// Float value, if `Float`.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            PropertyValue::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// Human-readable type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            PropertyValue::Null => "Null",
            PropertyValue::Bool(_) => "Boolean",
            PropertyValue::Int(_) => "Integer",
            PropertyValue::Float(_) => "Float",
            PropertyValue::String(_) => "String",
            PropertyValue::Bytes(_) => "Bytes",
            PropertyValue::List(_) => "List",
            PropertyValue::Map(_) => "Map",
            PropertyValue::DateTime(_) => "DateTime",
            PropertyValue::Duration(_) => "Duration",
            PropertyValue::Point2D { .. } => "Point2D",
            PropertyValue::Point3D { .. } => "Point3D",
            PropertyValue::Vector(_) => "Vector",
        }
    }

    /// String slice, if `String`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropertyValue::String(v) => Some(v),
            _ => None,
        }
    }
}

/// Property name -> value (query layer / CypherValue).
pub type PropertyMap = BTreeMap<String, PropertyValue>;

/// Property map with interned keys (stored on Node/Edge).
pub type InternedPropertyMap = super::compact_props::CompactPropertyStore;

/// Index key from a [`PropertyValue`]. Only scalars (Bool, Int, Float, String)
/// are indexable. `Ord` across types for B-trees: Bool < Int/Float < String.
#[derive(Debug, Clone)]
pub enum IndexKey {
    /// Boolean key.
    Bool(bool),
    /// Integer key.
    Int(i64),
    /// Float key.
    Float(f64),
    /// String key.
    String(String),
}

impl IndexKey {
    /// Index key of a value; `None` for Null, Bytes, List, Map and other non-scalars.
    pub fn from_property_value(pv: &PropertyValue) -> Option<Self> {
        match pv {
            PropertyValue::Bool(b) => Some(IndexKey::Bool(*b)),
            PropertyValue::Int(i) => Some(IndexKey::Int(*i)),
            PropertyValue::Float(f) => Some(IndexKey::Float(*f)),
            PropertyValue::String(s) => Some(IndexKey::String(s.clone())),
            _ => None,
        }
    }

    /// Encodes a tuple of component keys as ONE `IndexKey::String` for composite
    /// indexes. Unambiguous (type-tagged, strings length-prefixed), so `(1, "a")`
    /// never collides with `("1", "a")` or `("1\u{1F}a",)`:
    ///   `b:<0|1>` / `i:<dec>` / `f:<f64-bits-hex>` / `s:<len>:<bytes>`
    /// joined by `\u{1F}` (the separator composite-constraint descriptors use in
    /// their synthetic property names).
    pub fn composite_encode(components: &[IndexKey]) -> IndexKey {
        let mut out = String::new();
        for (i, c) in components.iter().enumerate() {
            if i > 0 {
                out.push('\u{1F}');
            }
            match c {
                IndexKey::Bool(b) => {
                    out.push_str(if *b { "b:1" } else { "b:0" });
                }
                IndexKey::Int(v) => {
                    out.push_str("i:");
                    out.push_str(&v.to_string());
                }
                IndexKey::Float(f) => {
                    out.push_str("f:");
                    out.push_str(&format!("{:016x}", f.to_bits()));
                }
                IndexKey::String(s) => {
                    out.push_str("s:");
                    out.push_str(&s.len().to_string());
                    out.push(':');
                    out.push_str(s);
                }
            }
        }
        IndexKey::String(out)
    }
}

impl PartialEq for IndexKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for IndexKey {}

impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (IndexKey::Bool(a), IndexKey::Bool(b)) => a.cmp(b),
            (IndexKey::Bool(_), _) => Ordering::Less,
            (_, IndexKey::Bool(_)) => Ordering::Greater,

            (IndexKey::Int(a), IndexKey::Int(b)) => a.cmp(b),
            (IndexKey::Int(a), IndexKey::Float(b)) => (*a as f64).total_cmp(b),
            (IndexKey::Float(a), IndexKey::Int(b)) => a.total_cmp(&(*b as f64)),
            (IndexKey::Float(a), IndexKey::Float(b)) => a.total_cmp(b),

            (IndexKey::Int(_) | IndexKey::Float(_), IndexKey::String(_)) => Ordering::Less,
            (IndexKey::String(_), IndexKey::Int(_) | IndexKey::Float(_)) => Ordering::Greater,

            (IndexKey::String(a), IndexKey::String(b)) => a.cmp(b),
        }
    }
}

impl std::hash::Hash for IndexKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            IndexKey::Bool(b) => b.hash(state),
            IndexKey::Int(i) => i.hash(state),
            IndexKey::Float(f) => f.to_bits().hash(state),
            IndexKey::String(s) => s.hash(state),
        }
    }
}
