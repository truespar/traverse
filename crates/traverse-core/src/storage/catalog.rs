//! Catalog metadata (string table, ID counters) serialized with rkyv into the catalog page.

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

use crate::error::{Error, Result};
use crate::storage::archive;

/// Catalog metadata persisted in the TVDB file.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct Catalog {
    /// Interned strings, by ID.
    pub strings: Vec<String>,
    /// Next node ID to allocate.
    pub next_node_id: u64,
    /// Next edge ID to allocate.
    pub next_edge_id: u64,
}

impl Catalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "catalog")
    }
}

/// Index definitions, catalog page slot 1. B-trees rebuild from node data on load.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct IndexCatalog {
    /// Index definitions: `(name, label, property_key)`.
    pub definitions: Vec<(String, String, String)>,
}

impl IndexCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "index catalog")
    }
}

/// Graph statistics, catalog page slot 2.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct StatsCatalog {
    /// Graph statistics.
    pub stats: crate::graph::stats::GraphStatistics,
}

impl StatsCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "stats catalog")
    }
}

/// Constraint definitions, catalog page slot 3.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct ConstraintCatalog {
    /// `(name, label, property_key, constraint_type)`.
    pub definitions: Vec<(String, String, String, String)>,
}

impl ConstraintCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "constraint catalog")
    }
}

/// Edge index definitions, catalog page slot 4.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct EdgeIndexCatalog {
    /// `(name, edge_type, property_key_or_empty)`; empty = type-only index.
    pub definitions: Vec<(String, String, String)>,
}

impl EdgeIndexCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "edge index catalog")
    }
}

/// Persisted vector-index descriptor. The HNSW isn't stored: it rebuilds on load from the persisted
/// node `LIST<FLOAT>` properties, like B-trees do.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct VectorIndexDef {
    /// Index name.
    pub name: String,
    /// Indexed node label.
    pub label: String,
    /// Indexed vector property.
    pub property: String,
    /// Vector dimensionality.
    pub dimensions: u64,
    /// Similarity function (`"cosine"` / `"euclidean"`).
    pub similarity: String,
    /// HNSW max neighbors per node (`m`).
    pub m: u64,
    /// HNSW construction beam width.
    pub ef_construction: u64,
    /// HNSW default query beam width.
    pub ef_search: u64,
    /// Level-assignment RNG seed (reproducible rebuilds).
    pub seed: u64,
    /// int8 scalar quantization enabled.
    pub quantize: bool,
}

/// Vector-index definitions, catalog page slot 5. Older files lack the slot,
/// older readers ignore it. HNSW rebuilds on load.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct VectorIndexCatalog {
    /// Vector-index descriptors.
    pub definitions: Vec<VectorIndexDef>,
}

impl VectorIndexCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "vector index catalog")
    }
}

/// Persisted full-text-index descriptor. Postings aren't stored: they rebuild on load from node
/// string properties.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct FulltextIndexDef {
    /// Index name.
    pub name: String,
    /// Indexed node labels (one or more).
    pub labels: Vec<String>,
    /// Indexed string properties (one or more).
    pub properties: Vec<String>,
    /// Analyzer name (`standard` / `english` / ...).
    pub analyzer: String,
}

/// Full-text-index definitions, catalog page slot 6. Older files lack the slot,
/// older readers ignore it. Postings rebuild on load.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct FulltextIndexCatalog {
    /// Full-text-index descriptors.
    pub definitions: Vec<FulltextIndexDef>,
}

impl FulltextIndexCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "fulltext index catalog")
    }
}

/// Persisted point-index descriptor. The R-tree isn't stored: it rebuilds on load from node point
/// properties.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct PointIndexDef {
    /// Index name.
    pub name: String,
    /// Indexed node label.
    pub label: String,
    /// Indexed point property.
    pub property: String,
}

/// Point-index definitions, catalog page slot 7. Older files lack the slot,
/// older readers ignore it. R-tree rebuilds on load.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct PointIndexCatalog {
    /// Point-index descriptors.
    pub definitions: Vec<PointIndexDef>,
}

impl PointIndexCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "point index catalog")
    }
}

/// Persisted text-index descriptor. Trigram postings aren't stored: they rebuild on load from node
/// string properties.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct TextIndexDef {
    /// Index name.
    pub name: String,
    /// Indexed node label.
    pub label: String,
    /// Indexed string property.
    pub property: String,
}

/// Text-index definitions, catalog page slot 8. Older files lack the slot, older
/// readers ignore it. Trigrams rebuild on load.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct TextIndexCatalog {
    /// Text-index descriptors.
    pub definitions: Vec<TextIndexDef>,
}

impl TextIndexCatalog {
    /// To rkyv bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|v| v.to_vec())
            .map_err(|e| Error::Serialization(e.to_string()))
    }

    /// From rkyv bytes, validated: damaged or crafted bytes give an error.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        archive::decode(data, "text index catalog")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_index_catalog_roundtrip() {
        let cat = VectorIndexCatalog {
            definitions: vec![VectorIndexDef {
                name: "docEmb".into(),
                label: "Doc".into(),
                property: "emb".into(),
                dimensions: 4096,
                similarity: "cosine".into(),
                m: 16,
                ef_construction: 100,
                ef_search: 100,
                seed: 0x9E3779B97F4A7C15,
                quantize: true,
            }],
        };
        let bytes = cat.to_bytes().unwrap();
        let restored = VectorIndexCatalog::from_bytes(&bytes).unwrap();
        assert_eq!(restored.definitions.len(), 1);
        let d = &restored.definitions[0];
        assert_eq!(d.name, "docEmb");
        assert_eq!(d.dimensions, 4096);
        assert_eq!(d.similarity, "cosine");
        assert!(d.quantize);
        assert_eq!(d.seed, 0x9E3779B97F4A7C15);
    }

    #[test]
    fn empty_catalog_roundtrip() {
        let catalog = Catalog {
            strings: Vec::new(),
            next_node_id: 0,
            next_edge_id: 0,
        };
        let bytes = catalog.to_bytes().unwrap();
        let restored = Catalog::from_bytes(&bytes).unwrap();
        assert!(restored.strings.is_empty());
        assert_eq!(restored.next_node_id, 0);
        assert_eq!(restored.next_edge_id, 0);
    }

    #[test]
    fn populated_catalog_roundtrip() {
        let catalog = Catalog {
            strings: vec![
                "Person".into(),
                "Movie".into(),
                "KNOWS".into(),
                "WATCHED".into(),
            ],
            next_node_id: 42,
            next_edge_id: 17,
        };
        let bytes = catalog.to_bytes().unwrap();
        let restored = Catalog::from_bytes(&bytes).unwrap();
        assert_eq!(restored.strings, catalog.strings);
        assert_eq!(restored.next_node_id, 42);
        assert_eq!(restored.next_edge_id, 17);
    }
}
