//! Cypher value types: [`CypherValue`] is [`PropertyValue`] plus the structural types (nodes,
//! edges, paths) query results need.

use std::collections::BTreeMap;
use std::fmt;

use traverse_core::graph::property::{PropertyMap, PropertyValue};
use traverse_core::graph::types::{EdgeId, EdgeTypeId, NodeId};

/// Value of a Cypher expression or query result: every storage type ([`PropertyValue`]) plus Node,
/// Edge and Path, which only exist at query time.
#[derive(Debug, Clone)]
pub enum CypherValue {
    /// Null / missing.
    Null,
    /// Boolean value.
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit floating-point number.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Ordered list.
    List(Vec<CypherValue>),
    /// Key-value map.
    Map(BTreeMap<String, CypherValue>),
    /// Graph node (from MATCH/CREATE).
    Node {
        /// Internal ID.
        id: NodeId,
        /// Label names.
        labels: Vec<String>,
        /// Properties.
        properties: PropertyMap,
    },
    /// Graph edge (from MATCH/CREATE).
    Edge {
        /// Internal ID.
        id: EdgeId,
        /// Type name.
        edge_type: String,
        /// Source node.
        source: NodeId,
        /// Target node.
        target: NodeId,
        /// Properties.
        properties: PropertyMap,
    },
    /// Lightweight node reference (properties resolved lazily).
    NodeRef(NodeId),
    /// Lightweight edge reference (properties resolved lazily).
    EdgeRef {
        /// Internal ID.
        id: EdgeId,
        /// Interned type ID (resolved lazily via StringTable).
        edge_type_id: EdgeTypeId,
        /// Source node.
        source: NodeId,
        /// Target node.
        target: NodeId,
    },
    /// Path: alternating nodes and edges.
    Path {
        /// Nodes along the path.
        nodes: Vec<CypherValue>,
        /// Edges along the path.
        edges: Vec<CypherValue>,
    },
    /// Byte array (Bolt BYTEARRAY; APOC arrow/compress payloads).
    Bytes(Vec<u8>),
    /// A date value.
    Date(crate::temporal::CypherDate),
    /// Local time (no time zone).
    LocalTime(crate::temporal::CypherLocalTime),
    /// Time with offset.
    Time(crate::temporal::CypherTime),
    /// Local datetime (no time zone).
    LocalDateTime(crate::temporal::CypherLocalDateTime),
    /// Datetime with time zone.
    DateTime(crate::temporal::CypherDateTime),
    /// A duration value.
    Duration(crate::temporal::CypherDuration),
}

impl CypherValue {
    /// Is `Null`?
    pub fn is_null(&self) -> bool {
        matches!(self, CypherValue::Null)
    }

    /// Bool value, if any.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            CypherValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Integer value, if any.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            CypherValue::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Float value, if any.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            CypherValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// String slice, if any.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            CypherValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Map, if any.
    pub fn as_map(&self) -> Option<&BTreeMap<String, CypherValue>> {
        match self {
            CypherValue::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Numeric value as f64; None for non-numeric types.
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            CypherValue::Integer(i) => Some(*i as f64),
            CypherValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Type name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            CypherValue::Null => "NULL",
            CypherValue::Bool(_) => "BOOLEAN",
            CypherValue::Integer(_) => "INTEGER",
            CypherValue::Float(_) => "FLOAT",
            CypherValue::String(_) => "STRING",
            CypherValue::List(_) => "LIST",
            CypherValue::Map(_) => "MAP",
            CypherValue::Node { .. } | CypherValue::NodeRef(_) => "NODE",
            CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => "RELATIONSHIP",
            CypherValue::Path { .. } => "PATH",
            CypherValue::Bytes(_) => "BYTEARRAY",
            CypherValue::Date(_) => "DATE",
            CypherValue::LocalTime(_) => "LOCAL_TIME",
            CypherValue::Time(_) => "TIME",
            CypherValue::LocalDateTime(_) => "LOCAL_DATE_TIME",
            CypherValue::DateTime(_) => "DATE_TIME",
            CypherValue::Duration(_) => "DURATION",
        }
    }

    /// `PropertyValue` -> `CypherValue`.
    #[recursive::recursive]
    pub fn from_property(pv: &PropertyValue) -> Self {
        match pv {
            PropertyValue::Null => CypherValue::Null,
            PropertyValue::Bool(b) => CypherValue::Bool(*b),
            PropertyValue::Int(i) => CypherValue::Integer(*i),
            PropertyValue::Float(f) => CypherValue::Float(*f),
            PropertyValue::String(s) => {
                // tagged temporal strings from storage round-trips
                if let Some(rest) = s.strip_prefix("__cypher_date:") {
                    if let Ok(cv) = crate::temporal::parse_date_string(rest) {
                        return cv;
                    }
                } else if let Some(rest) = s.strip_prefix("__cypher_localtime:") {
                    if let Ok(cv) = crate::temporal::parse_localtime_string(rest) {
                        return cv;
                    }
                } else if let Some(rest) = s.strip_prefix("__cypher_time:") {
                    if let Ok(cv) = crate::temporal::parse_time_string(rest) {
                        return cv;
                    }
                } else if let Some(rest) = s.strip_prefix("__cypher_localdatetime:") {
                    if let Ok(cv) = crate::temporal::parse_localdatetime_string(rest) {
                        return cv;
                    }
                } else if let Some(rest) = s.strip_prefix("__cypher_datetime:") {
                    if let Ok(cv) = crate::temporal::parse_datetime_string(rest) {
                        return cv;
                    }
                } else if let Some(rest) = s.strip_prefix("__cypher_duration:")
                    && let Ok(cv) = crate::temporal::parse_duration_string(rest)
                {
                    return cv;
                }
                CypherValue::String(s.clone())
            }
            PropertyValue::Bytes(b) => CypherValue::Bytes(b.clone()),
            PropertyValue::List(l) => {
                CypherValue::List(l.iter().map(CypherValue::from_property).collect())
            }
            PropertyValue::Map(m) => CypherValue::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), CypherValue::from_property(v)))
                    .collect(),
            ),
            PropertyValue::DateTime(ms) => CypherValue::Integer(*ms),
            PropertyValue::Duration(ms) => CypherValue::Integer(*ms),
            PropertyValue::Point2D { x, y, srid } => {
                let mut m = BTreeMap::new();
                m.insert("x".to_string(), CypherValue::Float(*x));
                m.insert("y".to_string(), CypherValue::Float(*y));
                m.insert("srid".to_string(), CypherValue::Integer(*srid as i64));
                m.insert(
                    "crs".to_string(),
                    CypherValue::String(crs_of_srid(*srid).to_string()),
                );
                CypherValue::Map(m)
            }
            PropertyValue::Point3D { x, y, z, srid } => {
                let mut m = BTreeMap::new();
                m.insert("x".to_string(), CypherValue::Float(*x));
                m.insert("y".to_string(), CypherValue::Float(*y));
                m.insert("z".to_string(), CypherValue::Float(*z));
                m.insert("srid".to_string(), CypherValue::Integer(*srid as i64));
                m.insert(
                    "crs".to_string(),
                    CypherValue::String(crs_of_srid(*srid).to_string()),
                );
                CypherValue::Map(m)
            }
            PropertyValue::Vector(v) => {
                CypherValue::List(v.iter().map(|&f| CypherValue::Float(f as f64)).collect())
            }
        }
    }

    /// `CypherValue` -> `PropertyValue`; `None` for structural types (Node, Edge, Path).
    pub fn to_property(&self) -> Option<PropertyValue> {
        match self {
            CypherValue::Null => Some(PropertyValue::Null),
            CypherValue::Bool(b) => Some(PropertyValue::Bool(*b)),
            CypherValue::Integer(i) => Some(PropertyValue::Int(*i)),
            CypherValue::Float(f) => Some(PropertyValue::Float(*f)),
            CypherValue::String(s) => Some(PropertyValue::String(s.clone())),
            CypherValue::List(l) => {
                let items: Option<Vec<_>> = l.iter().map(|v| v.to_property()).collect();
                items.map(PropertyValue::List)
            }
            CypherValue::Map(m) => {
                // canonical point maps (what point() builds) store as REAL spatial values: they
                // round-trip through from_property and go over Bolt as Point structures
                if let (
                    Some(CypherValue::Float(x)),
                    Some(CypherValue::Float(y)),
                    Some(CypherValue::Integer(srid)),
                ) = (m.get("x"), m.get("y"), m.get("srid"))
                {
                    let point_keys = m.keys().all(|k| {
                        matches!(
                            k.as_str(),
                            "x" | "y" | "z" | "srid" | "crs" | "latitude" | "longitude" | "height"
                        )
                    });
                    if point_keys {
                        return Some(match m.get("z") {
                            Some(CypherValue::Float(z)) => PropertyValue::Point3D {
                                x: *x,
                                y: *y,
                                z: *z,
                                srid: *srid as u32,
                            },
                            _ => PropertyValue::Point2D {
                                x: *x,
                                y: *y,
                                srid: *srid as u32,
                            },
                        });
                    }
                }
                let items: Option<BTreeMap<_, _>> = m
                    .iter()
                    .map(|(k, v)| v.to_property().map(|pv| (k.clone(), pv)))
                    .collect();
                items.map(PropertyValue::Map)
            }
            CypherValue::Date(d) => Some(PropertyValue::String(format!("__cypher_date:{d}"))),
            CypherValue::LocalTime(t) => {
                Some(PropertyValue::String(format!("__cypher_localtime:{t}")))
            }
            CypherValue::Time(t) => Some(PropertyValue::String(format!("__cypher_time:{t}"))),
            CypherValue::LocalDateTime(ldt) => Some(PropertyValue::String(format!(
                "__cypher_localdatetime:{ldt}"
            ))),
            CypherValue::DateTime(dt) => {
                Some(PropertyValue::String(format!("__cypher_datetime:{dt}")))
            }
            CypherValue::Duration(d) => {
                Some(PropertyValue::String(format!("__cypher_duration:{d}")))
            }
            CypherValue::Bytes(b) => Some(PropertyValue::Bytes(b.clone())),
            CypherValue::Node { .. }
            | CypherValue::Edge { .. }
            | CypherValue::NodeRef(_)
            | CypherValue::EdgeRef { .. }
            | CypherValue::Path { .. } => None,
        }
    }
}

/// Neo4j CRS names for the standard SRIDs.
pub fn crs_of_srid(srid: u32) -> &'static str {
    match srid {
        4326 => "wgs-84",
        4979 => "wgs-84-3d",
        9157 => "cartesian-3d",
        _ => "cartesian",
    }
}

impl fmt::Display for CypherValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CypherValue::Null => write!(f, "null"),
            CypherValue::Bytes(b) => {
                // hex, like Neo4j's byte-array toString
                for byte in b {
                    write!(f, "{byte:02X}")?;
                }
                Ok(())
            }
            CypherValue::Bool(b) => write!(f, "{b}"),
            CypherValue::Integer(i) => write!(f, "{i}"),
            CypherValue::Float(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            CypherValue::String(s) => write!(f, "\"{s}\""),
            CypherValue::List(l) => {
                write!(f, "[")?;
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            CypherValue::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            CypherValue::Node {
                id,
                labels,
                properties,
            } => {
                write!(f, "(id={}", id.0)?;
                for l in labels {
                    write!(f, ":{l}")?;
                }
                if !properties.is_empty() {
                    write!(f, " {{")?;
                    for (i, (k, v)) in properties.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{k}: ")?;
                        write!(f, "{}", CypherValue::from_property(v))?;
                    }
                    write!(f, "}}")?;
                }
                write!(f, ")")
            }
            CypherValue::Edge {
                id,
                edge_type,
                source,
                target,
                properties,
            } => {
                write!(f, "[id={}:{edge_type} {}->{}", id.0, source.0, target.0)?;
                if !properties.is_empty() {
                    write!(f, " {{")?;
                    for (i, (k, v)) in properties.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{k}: ")?;
                        write!(f, "{}", CypherValue::from_property(v))?;
                    }
                    write!(f, "}}")?;
                }
                write!(f, "]")
            }
            CypherValue::NodeRef(id) => write!(f, "(id={})", id.0),
            CypherValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => {
                write!(
                    f,
                    "[id={}:etype#{} {}->{}]",
                    id.0, edge_type_id.0, source.0, target.0
                )
            }
            CypherValue::Path { nodes, edges } => {
                write!(f, "<")?;
                for (i, n) in nodes.iter().enumerate() {
                    if i > 0
                        && let Some(e) = edges.get(i - 1)
                    {
                        write!(f, "-{e}-")?;
                    }
                    write!(f, "{n}")?;
                }
                write!(f, ">")
            }
            CypherValue::Date(d) => write!(f, "{d}"),
            CypherValue::LocalTime(t) => write!(f, "{t}"),
            CypherValue::Time(t) => write!(f, "{t}"),
            CypherValue::LocalDateTime(ldt) => write!(f, "{ldt}"),
            CypherValue::DateTime(dt) => write!(f, "{dt}"),
            CypherValue::Duration(d) => write!(f, "{d}"),
        }
    }
}

impl PartialEq for CypherValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CypherValue::Null, CypherValue::Null) => true,
            (CypherValue::Bool(a), CypherValue::Bool(b)) => a == b,
            (CypherValue::Integer(a), CypherValue::Integer(b)) => a == b,
            (CypherValue::Float(a), CypherValue::Float(b)) => a == b,
            (CypherValue::Integer(a), CypherValue::Float(b)) => (*a as f64) == *b,
            (CypherValue::Float(a), CypherValue::Integer(b)) => *a == (*b as f64),
            (CypherValue::String(a), CypherValue::String(b)) => a == b,
            (CypherValue::List(a), CypherValue::List(b)) => a == b,
            (CypherValue::Map(a), CypherValue::Map(b)) => a == b,
            (CypherValue::Node { id: a, .. }, CypherValue::Node { id: b, .. }) => a == b,
            (CypherValue::NodeRef(a), CypherValue::NodeRef(b)) => a == b,
            (CypherValue::Node { id, .. }, CypherValue::NodeRef(b)) => id == b,
            (CypherValue::NodeRef(a), CypherValue::Node { id, .. }) => a == id,
            (CypherValue::Edge { id: a, .. }, CypherValue::Edge { id: b, .. }) => a == b,
            (CypherValue::EdgeRef { id: a, .. }, CypherValue::EdgeRef { id: b, .. }) => a == b,
            (CypherValue::Edge { id: a, .. }, CypherValue::EdgeRef { id: b, .. }) => a == b,
            (CypherValue::EdgeRef { id: a, .. }, CypherValue::Edge { id: b, .. }) => a == b,
            (
                CypherValue::Path {
                    nodes: n1,
                    edges: e1,
                },
                CypherValue::Path {
                    nodes: n2,
                    edges: e2,
                },
            ) => n1 == n2 && e1 == e2,
            (CypherValue::Date(a), CypherValue::Date(b)) => a == b,
            (CypherValue::LocalTime(a), CypherValue::LocalTime(b)) => a == b,
            (CypherValue::Time(a), CypherValue::Time(b)) => a == b,
            (CypherValue::LocalDateTime(a), CypherValue::LocalDateTime(b)) => a == b,
            (CypherValue::DateTime(a), CypherValue::DateTime(b)) => a == b,
            (CypherValue::Duration(a), CypherValue::Duration(b)) => a == b,
            _ => false,
        }
    }
}

impl PartialOrd for CypherValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (CypherValue::Null, CypherValue::Null) => Some(std::cmp::Ordering::Equal),
            (CypherValue::Bool(a), CypherValue::Bool(b)) => a.partial_cmp(b),
            (CypherValue::Integer(a), CypherValue::Integer(b)) => a.partial_cmp(b),
            (CypherValue::Float(a), CypherValue::Float(b)) => a.partial_cmp(b),
            (CypherValue::Integer(a), CypherValue::Float(b)) => (*a as f64).partial_cmp(b),
            (CypherValue::Float(a), CypherValue::Integer(b)) => a.partial_cmp(&(*b as f64)),
            (CypherValue::String(a), CypherValue::String(b)) => a.partial_cmp(b),
            (CypherValue::Date(a), CypherValue::Date(b)) => a.partial_cmp(b),
            (CypherValue::LocalTime(a), CypherValue::LocalTime(b)) => a.partial_cmp(b),
            (CypherValue::Time(a), CypherValue::Time(b)) => a.partial_cmp(b),
            (CypherValue::LocalDateTime(a), CypherValue::LocalDateTime(b)) => a.partial_cmp(b),
            (CypherValue::DateTime(a), CypherValue::DateTime(b)) => a.partial_cmp(b),
            (CypherValue::List(a), CypherValue::List(b)) => {
                for (ai, bi) in a.iter().zip(b.iter()) {
                    match ai.partial_cmp(bi) {
                        Some(std::cmp::Ordering::Equal) => continue,
                        other => return other,
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => None,
        }
    }
}
