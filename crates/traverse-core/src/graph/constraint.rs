//! Constraint registry: uniqueness, existence, property type and node key.

use rustc_hash::FxHashSet;

use super::types::{LabelId, PropertyKeyId};
use crate::error::{Error, Result};

/// Constraint kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintType {
    /// No two nodes with the label share the property value.
    Unique,
    /// Every node with the label has the property (non-null).
    Exists,
    /// Values, when set, must have the named Cypher type. NULL always passes (add `Exists` for
    /// non-null).
    Type(PropertyTypeKind),
    /// Node key: uniqueness + existence. Composite keys carry the extra properties in
    /// `extra_property_keys`.
    NodeKey,
}

/// Cypher property type as in `REQUIRE n.p IS :: <TYPE>` (Neo4j 5 property-type vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyTypeKind {
    /// `STRING`
    String,
    /// `INTEGER` (Cypher 64-bit integer)
    Integer,
    /// `FLOAT` (64-bit double)
    Float,
    /// `BOOLEAN`
    Boolean,
    /// `POINT`
    Point,
    /// `DATE`
    Date,
    /// `LOCAL TIME`
    LocalTime,
    /// `ZONED TIME`
    ZonedTime,
    /// `LOCAL DATETIME`
    LocalDatetime,
    /// `ZONED DATETIME`
    ZonedDatetime,
    /// `DURATION`
    Duration,
    /// `LIST<*>`: any list
    List,
}

impl PropertyTypeKind {
    /// Canonical spelling (`SHOW CONSTRAINTS`, serialization).
    pub fn surface(self) -> &'static str {
        match self {
            PropertyTypeKind::String => "STRING",
            PropertyTypeKind::Integer => "INTEGER",
            PropertyTypeKind::Float => "FLOAT",
            PropertyTypeKind::Boolean => "BOOLEAN",
            PropertyTypeKind::Point => "POINT",
            PropertyTypeKind::Date => "DATE",
            PropertyTypeKind::LocalTime => "LOCAL TIME",
            PropertyTypeKind::ZonedTime => "ZONED TIME",
            PropertyTypeKind::LocalDatetime => "LOCAL DATETIME",
            PropertyTypeKind::ZonedDatetime => "ZONED DATETIME",
            PropertyTypeKind::Duration => "DURATION",
            PropertyTypeKind::List => "LIST",
        }
    }

    /// Whether `value` has this type. NULL always matches (as in Neo4j; add `Exists` to force
    /// non-null).
    pub fn matches(self, value: &super::property::PropertyValue) -> bool {
        use super::property::PropertyValue as P;
        match (self, value) {
            (_, P::Null) => true,
            (PropertyTypeKind::String, P::String(_)) => true,
            (PropertyTypeKind::Integer, P::Int(_)) => true,
            (PropertyTypeKind::Float, P::Float(_)) => true,
            (PropertyTypeKind::Boolean, P::Bool(_)) => true,
            (PropertyTypeKind::Point, P::Point2D { .. } | P::Point3D { .. }) => true,
            // `DateTime` storage covers DATETIME / LOCAL DATETIME / DATE and can't tell
            // them apart, so any temporal value passes any date/time kind
            (
                PropertyTypeKind::Date
                | PropertyTypeKind::LocalTime
                | PropertyTypeKind::ZonedTime
                | PropertyTypeKind::LocalDatetime
                | PropertyTypeKind::ZonedDatetime,
                P::DateTime(_),
            ) => true,
            (PropertyTypeKind::Duration, P::Duration(_)) => true,
            (PropertyTypeKind::List, P::List(_) | P::Vector(_)) => true,
            _ => false,
        }
    }

    /// Parses the surface spelling (any case, spaces normalised).
    pub fn from_surface(s: &str) -> Option<Self> {
        let upper: String = s
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();
        Some(match upper.as_str() {
            "STRING" => PropertyTypeKind::String,
            "INTEGER" | "INT" => PropertyTypeKind::Integer,
            "FLOAT" => PropertyTypeKind::Float,
            "BOOLEAN" | "BOOL" => PropertyTypeKind::Boolean,
            "POINT" => PropertyTypeKind::Point,
            "DATE" => PropertyTypeKind::Date,
            "LOCAL TIME" => PropertyTypeKind::LocalTime,
            "ZONED TIME" => PropertyTypeKind::ZonedTime,
            "LOCAL DATETIME" => PropertyTypeKind::LocalDatetime,
            "ZONED DATETIME" => PropertyTypeKind::ZonedDatetime,
            "DURATION" => PropertyTypeKind::Duration,
            "LIST" => PropertyTypeKind::List,
            // a VECTOR is a LIST<FLOAT> here, so the VECTOR constraint enforces List
            "VECTOR" => PropertyTypeKind::List,
            _ => return None,
        })
    }
}

/// Whether a constraint applies to nodes (by label) or relationships (by type).
/// Labels and types share one interned ID space, so this keeps a node constraint
/// on `Foo` apart from a relationship constraint on type `Foo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConstraintEntity {
    /// Node constraint, keyed by label.
    #[default]
    Node,
    /// Relationship constraint, keyed by relationship type.
    Relationship,
}

/// Constraint metadata.
#[derive(Debug, Clone)]
pub struct ConstraintDescriptor {
    /// User-visible constraint name.
    pub name: String,
    /// Token the constraint is keyed on: label ID (node) or relationship-type ID (relationship);
    /// same string table, told apart by `entity`.
    pub label_id: LabelId,
    /// Constrained property key. Composite constraints (`REQUIRE (n.a, n.b) IS NODE KEY` / `IS
    /// UNIQUE`): the FIRST key, the rest are in `extra_property_keys`.
    pub property_key: PropertyKeyId,
    /// Further keys of a composite constraint (empty otherwise). Existence = every key present;
    /// uniqueness = the value TUPLE is unique.
    pub extra_property_keys: Vec<PropertyKeyId>,
    /// Constraint kind.
    pub constraint_type: ConstraintType,
    /// Nodes (by label) or relationships (by type).
    pub entity: ConstraintEntity,
}

/// All constraints of a graph.
#[derive(Debug, Clone, Default)]
pub struct ConstraintRegistry {
    constraints: Vec<ConstraintDescriptor>,
    name_set: FxHashSet<String>,
}

impl ConstraintRegistry {
    /// Registers a constraint. Errors on a taken name or a duplicate (label, property, type).
    pub fn create_constraint(&mut self, desc: ConstraintDescriptor) -> Result<()> {
        if self.name_set.contains(&desc.name) {
            return Err(Error::ConstraintError(format!(
                "constraint '{}' already exists",
                desc.name
            )));
        }
        // duplicate (entity, label/type, prop, type)?
        for existing in &self.constraints {
            if existing.entity == desc.entity
                && existing.label_id == desc.label_id
                && existing.property_key == desc.property_key
                && existing.constraint_type == desc.constraint_type
            {
                return Err(Error::ConstraintError(format!(
                    "a {} constraint already exists for that label/property combination",
                    match &desc.constraint_type {
                        ConstraintType::Unique => "unique".to_string(),
                        ConstraintType::Exists => "exists".to_string(),
                        ConstraintType::Type(k) => format!("type ({})", k.surface()),
                        ConstraintType::NodeKey => "NODE KEY".to_string(),
                    }
                )));
            }
        }
        self.name_set.insert(desc.name.clone());
        self.constraints.push(desc);
        Ok(())
    }

    /// Drops a constraint by name; errors if missing.
    pub fn drop_constraint(&mut self, name: &str) -> Result<()> {
        let pos = self
            .constraints
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| Error::ConstraintError(format!("constraint '{name}' not found")))?;
        self.constraints.remove(pos);
        self.name_set.remove(name);
        Ok(())
    }

    /// Descriptor by name.
    pub fn get_by_name(&self, name: &str) -> Option<&ConstraintDescriptor> {
        self.constraints.iter().find(|c| c.name == name)
    }

    /// Unique-flavoured constraints on `label` (NODE KEY included: it enforces uniqueness too).
    pub fn unique_constraints_for_label(
        &self,
        label_id: LabelId,
    ) -> impl Iterator<Item = &ConstraintDescriptor> {
        self.constraints.iter().filter(move |c| {
            c.entity == ConstraintEntity::Node
                && c.label_id == label_id
                && matches!(
                    c.constraint_type,
                    ConstraintType::Unique | ConstraintType::NodeKey
                )
        })
    }

    /// [`Self::unique_constraints_for_label`] for a relationship type.
    pub fn unique_constraints_for_edge_type(
        &self,
        type_id: LabelId,
    ) -> impl Iterator<Item = &ConstraintDescriptor> {
        self.constraints.iter().filter(move |c| {
            c.entity == ConstraintEntity::Relationship
                && c.label_id == type_id
                && matches!(
                    c.constraint_type,
                    ConstraintType::Unique | ConstraintType::NodeKey
                )
        })
    }

    /// [`Self::existence_constraints_for_label`] for a relationship type.
    pub fn existence_constraints_for_edge_type(
        &self,
        type_id: LabelId,
    ) -> impl Iterator<Item = &ConstraintDescriptor> {
        self.constraints.iter().filter(move |c| {
            c.entity == ConstraintEntity::Relationship
                && c.label_id == type_id
                && matches!(
                    c.constraint_type,
                    ConstraintType::Exists | ConstraintType::NodeKey
                )
        })
    }

    /// [`Self::type_constraints_for_label`] for a relationship type.
    pub fn type_constraints_for_edge_type(
        &self,
        type_id: LabelId,
    ) -> impl Iterator<Item = &ConstraintDescriptor> {
        self.constraints.iter().filter(move |c| {
            c.entity == ConstraintEntity::Relationship
                && c.label_id == type_id
                && matches!(c.constraint_type, ConstraintType::Type(_))
        })
    }

    /// Existence-flavoured constraints on `label` (NODE KEY included: it enforces non-null too).
    pub fn existence_constraints_for_label(
        &self,
        label_id: LabelId,
    ) -> impl Iterator<Item = &ConstraintDescriptor> {
        self.constraints.iter().filter(move |c| {
            c.entity == ConstraintEntity::Node
                && c.label_id == label_id
                && matches!(
                    c.constraint_type,
                    ConstraintType::Exists | ConstraintType::NodeKey
                )
        })
    }

    /// Unique constraint on `(label, prop)`? NODE KEY counts.
    pub fn has_unique_constraint(&self, label_id: LabelId, prop: PropertyKeyId) -> bool {
        self.constraints.iter().any(|c| {
            c.entity == ConstraintEntity::Node
                && c.label_id == label_id
                && c.property_key == prop
                && matches!(
                    c.constraint_type,
                    ConstraintType::Unique | ConstraintType::NodeKey
                )
        })
    }

    /// Existence constraint on `(label, prop)`? NODE KEY counts.
    pub fn has_existence_constraint(&self, label_id: LabelId, prop: PropertyKeyId) -> bool {
        self.constraints.iter().any(|c| {
            c.entity == ConstraintEntity::Node
                && c.label_id == label_id
                && c.property_key == prop
                && matches!(
                    c.constraint_type,
                    ConstraintType::Exists | ConstraintType::NodeKey
                )
        })
    }

    /// Type constraints on a node label.
    pub fn type_constraints_for_label(
        &self,
        label_id: LabelId,
    ) -> impl Iterator<Item = &ConstraintDescriptor> {
        self.constraints.iter().filter(move |c| {
            c.entity == ConstraintEntity::Node
                && c.label_id == label_id
                && matches!(c.constraint_type, ConstraintType::Type(_))
        })
    }

    /// All descriptors.
    pub fn descriptors(&self) -> &[ConstraintDescriptor] {
        &self.constraints
    }

    /// No constraints registered?
    pub fn is_empty(&self) -> bool {
        self.constraints.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(name: &str, lid: u32, pkid: u32, ctype: ConstraintType) -> ConstraintDescriptor {
        ConstraintDescriptor {
            name: name.to_string(),
            label_id: LabelId(lid),
            property_key: PropertyKeyId(pkid),
            constraint_type: ctype,
            extra_property_keys: Vec::new(),
            entity: ConstraintEntity::Node,
        }
    }

    #[test]
    fn create_and_drop() {
        let mut reg = ConstraintRegistry::default();
        reg.create_constraint(desc("c1", 0, 1, ConstraintType::Unique))
            .unwrap();
        assert!(!reg.is_empty());
        assert!(reg.get_by_name("c1").is_some());
        reg.drop_constraint("c1").unwrap();
        assert!(reg.is_empty());
        assert!(reg.get_by_name("c1").is_none());
    }

    #[test]
    fn duplicate_name_error() {
        let mut reg = ConstraintRegistry::default();
        reg.create_constraint(desc("c1", 0, 1, ConstraintType::Unique))
            .unwrap();
        let err = reg
            .create_constraint(desc("c1", 2, 3, ConstraintType::Exists))
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn duplicate_label_prop_type_error() {
        let mut reg = ConstraintRegistry::default();
        reg.create_constraint(desc("c1", 0, 1, ConstraintType::Unique))
            .unwrap();
        let err = reg
            .create_constraint(desc("c2", 0, 1, ConstraintType::Unique))
            .unwrap_err();
        assert!(err.to_string().contains("unique constraint already exists"));
    }

    #[test]
    fn same_label_prop_different_type_ok() {
        let mut reg = ConstraintRegistry::default();
        reg.create_constraint(desc("c1", 0, 1, ConstraintType::Unique))
            .unwrap();
        reg.create_constraint(desc("c2", 0, 1, ConstraintType::Exists))
            .unwrap();
        assert_eq!(reg.descriptors().len(), 2);
    }

    #[test]
    fn drop_nonexistent_error() {
        let mut reg = ConstraintRegistry::default();
        let err = reg.drop_constraint("ghost").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn lookup_by_label() {
        let mut reg = ConstraintRegistry::default();
        reg.create_constraint(desc("u1", 0, 1, ConstraintType::Unique))
            .unwrap();
        reg.create_constraint(desc("e1", 0, 2, ConstraintType::Exists))
            .unwrap();
        reg.create_constraint(desc("u2", 1, 1, ConstraintType::Unique))
            .unwrap();

        let uniq: Vec<_> = reg.unique_constraints_for_label(LabelId(0)).collect();
        assert_eq!(uniq.len(), 1);
        assert_eq!(uniq[0].name, "u1");

        let exist: Vec<_> = reg.existence_constraints_for_label(LabelId(0)).collect();
        assert_eq!(exist.len(), 1);
        assert_eq!(exist[0].name, "e1");
    }

    #[test]
    fn has_unique_and_existence() {
        let mut reg = ConstraintRegistry::default();
        reg.create_constraint(desc("u1", 0, 1, ConstraintType::Unique))
            .unwrap();
        reg.create_constraint(desc("e1", 0, 2, ConstraintType::Exists))
            .unwrap();

        assert!(reg.has_unique_constraint(LabelId(0), PropertyKeyId(1)));
        assert!(!reg.has_unique_constraint(LabelId(0), PropertyKeyId(2)));
        assert!(reg.has_existence_constraint(LabelId(0), PropertyKeyId(2)));
        assert!(!reg.has_existence_constraint(LabelId(0), PropertyKeyId(1)));
    }
}
