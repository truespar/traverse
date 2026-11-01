//! Shared config parsing: orientation, label / type filters, concurrency, weight property, write
//! target. Algorithm-specific config sits on top.

use std::collections::BTreeMap;

use crate::types::CypherValue;

use super::AlgorithmError;

/// Algorithm config map parse errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Required key missing.
    #[error("config key `{0}` is required")]
    Missing(&'static str),
    /// Key holds the wrong type.
    #[error("config key `{key}` has wrong type (expected {expected}, got {actual})")]
    WrongType {
        /// Config key.
        key: &'static str,
        /// Expected type.
        expected: &'static str,
        /// Type the caller supplied.
        actual: &'static str,
    },
    /// Right type, but out of range; key plus explanation.
    #[error("config key `{0}` has invalid value: {1}")]
    Invalid(&'static str, String),
}

impl From<ConfigError> for AlgorithmError {
    fn from(e: ConfigError) -> Self {
        AlgorithmError::Config(e.to_string())
    }
}

/// Traversal direction during projection; GDS's `orientation` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// Stored direction (out edges of `from`).
    #[default]
    Natural,
    /// Reversed (in edges of `from`).
    Reverse,
    /// Undirected: out union in per node, deduped by edge id.
    Undirected,
}

impl Orientation {
    /// Parses `NATURAL`, `REVERSE` or `UNDIRECTED`, any case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "NATURAL" => Some(Orientation::Natural),
            "REVERSE" => Some(Orientation::Reverse),
            "UNDIRECTED" => Some(Orientation::Undirected),
            _ => None,
        }
    }

    /// Upper-case GDS spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Orientation::Natural => "NATURAL",
            Orientation::Reverse => "REVERSE",
            Orientation::Undirected => "UNDIRECTED",
        }
    }
}

/// Keys every algorithm understands. Specific configs usually embed this and add their own
/// (`damping`, `maxIterations`, ...).
#[derive(Debug, Clone, Default)]
pub struct BaseConfig {
    /// Only nodes with any of these labels; `None` = all.
    pub node_labels: Option<Vec<String>>,
    /// Only edges of any of these types; `None` = all.
    pub relationship_types: Option<Vec<String>>,
    /// Traversal direction.
    pub orientation: Orientation,
    /// Edge weight property; `None` = unweighted.
    pub relationship_weight_property: Option<String>,
    /// `0` = rayon's global default thread count.
    pub concurrency: usize,
    /// `.write` target property; required for `.write`, ignored otherwise.
    pub write_property: Option<String>,
}

impl BaseConfig {
    /// Parses the well-known keys from a Cypher map; unknown keys are left to the
    /// algorithm-specific parser.
    pub fn parse(params: &BTreeMap<String, CypherValue>) -> Result<Self, ConfigError> {
        let mut cfg = BaseConfig::default();
        if let Some(v) = params.get("nodeLabels") {
            cfg.node_labels = Some(parse_string_list("nodeLabels", v)?);
        }
        if let Some(v) = params.get("relationshipTypes") {
            cfg.relationship_types = Some(parse_string_list("relationshipTypes", v)?);
        }
        if let Some(v) = params.get("orientation") {
            let s = parse_string("orientation", v)?;
            cfg.orientation = Orientation::parse(&s).ok_or_else(|| {
                ConfigError::Invalid(
                    "orientation",
                    format!("expected NATURAL|REVERSE|UNDIRECTED, got {s}"),
                )
            })?;
        }
        if let Some(v) = params.get("relationshipWeightProperty") {
            cfg.relationship_weight_property = Some(parse_string("relationshipWeightProperty", v)?);
        }
        if let Some(v) = params.get("concurrency") {
            cfg.concurrency = parse_integer("concurrency", v)?.max(0) as usize;
        }
        if let Some(v) = params.get("writeProperty") {
            cfg.write_property = Some(parse_string("writeProperty", v)?);
        }
        Ok(cfg)
    }
}

// -- typed extractors -----------------------------------------------------

/// `v` as a boolean, or an error naming `key` and the actual type.
pub fn parse_bool(key: &'static str, v: &CypherValue) -> Result<bool, ConfigError> {
    match v {
        CypherValue::Bool(b) => Ok(*b),
        other => Err(ConfigError::WrongType {
            key,
            expected: "bool",
            actual: other.type_name(),
        }),
    }
}

/// `v` as an integer; floats are rejected, not truncated.
pub fn parse_integer(key: &'static str, v: &CypherValue) -> Result<i64, ConfigError> {
    match v {
        CypherValue::Integer(i) => Ok(*i),
        other => Err(ConfigError::WrongType {
            key,
            expected: "integer",
            actual: other.type_name(),
        }),
    }
}

/// `v` as a float; integers widen.
pub fn parse_float(key: &'static str, v: &CypherValue) -> Result<f64, ConfigError> {
    match v {
        CypherValue::Float(f) => Ok(*f),
        CypherValue::Integer(i) => Ok(*i as f64),
        other => Err(ConfigError::WrongType {
            key,
            expected: "float",
            actual: other.type_name(),
        }),
    }
}

/// `v` as a string.
pub fn parse_string(key: &'static str, v: &CypherValue) -> Result<String, ConfigError> {
    match v {
        CypherValue::String(s) => Ok(s.clone()),
        other => Err(ConfigError::WrongType {
            key,
            expected: "string",
            actual: other.type_name(),
        }),
    }
}

/// `v` as a list of strings.
pub fn parse_string_list(key: &'static str, v: &CypherValue) -> Result<Vec<String>, ConfigError> {
    match v {
        CypherValue::List(items) => items
            .iter()
            .map(|it| match it {
                CypherValue::String(s) => Ok(s.clone()),
                other => Err(ConfigError::WrongType {
                    key,
                    expected: "list of strings",
                    actual: other.type_name(),
                }),
            })
            .collect(),
        other => Err(ConfigError::WrongType {
            key,
            expected: "list of strings",
            actual: other.type_name(),
        }),
    }
}

/// Boolean at `key`, or `default` when absent.
pub fn opt_bool(
    params: &BTreeMap<String, CypherValue>,
    key: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    params
        .get(key)
        .map(|v| parse_bool(key, v))
        .unwrap_or(Ok(default))
}

/// Integer at `key`, or `default` when absent.
pub fn opt_integer(
    params: &BTreeMap<String, CypherValue>,
    key: &'static str,
    default: i64,
) -> Result<i64, ConfigError> {
    params
        .get(key)
        .map(|v| parse_integer(key, v))
        .unwrap_or(Ok(default))
}

/// Float at `key` (integers widen), or `default` when absent.
pub fn opt_float(
    params: &BTreeMap<String, CypherValue>,
    key: &'static str,
    default: f64,
) -> Result<f64, ConfigError> {
    params
        .get(key)
        .map(|v| parse_float(key, v))
        .unwrap_or(Ok(default))
}
