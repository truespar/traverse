//! Bytes-only Graph <-> PageManager codec.
//!
//! Core of the `.tvdb` format, split out of
//! [`crate::storage::database::Database`] so the WASM bindings can work on byte
//! buffers without `std::fs::File`.
//!
//! * [`load_graph`]: fully loaded [`PageManager`] -> [`Graph`] + [`LocationIndex`]
//!   + the PageManager back (native keeps it for incremental flushes, WASM drops it).
//! * [`write_graph`]: fills an empty PageManager with a Graph + optional
//!   metadata. Then `pm.flush()` (native) or `pm.to_bytes()` for a flat
//!   Uint8Array (WASM / OPFS). [`write_checkpoint`] does the same from a
//!   [`Checkpoint`] of a running store.
//!
//! Native `Database` uses the same functions, so the format is identical: a tvdb
//! written by the server loads in a browser via `TraverseDb.loadTvdb` and vice versa.

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::graph::edge::Edge;
use crate::graph::node::Node;
use crate::graph::types::{EdgeId, EdgeTypeId, NodeId};
use crate::graph::{CatalogParts, Graph};
use crate::storage::archive::{self, Aligned};
use crate::storage::catalog::{self, Catalog};
use crate::storage::compression;
use crate::storage::format::PAGE_DATA_CAPACITY;
use crate::storage::location::{LocationIndex, RecordLocation};
use crate::storage::overflow;
use crate::storage::page::{PageType, SLOT_ENTRY_SIZE, SlottedPage, SlottedPageRef};
use crate::storage::page_manager::PageManager;
use crate::tx::Checkpoint;

/// The record in `slot` of a page, read through its overflow pages if it continues on them, and
/// decoded; `None` when the page has no such slot (older files) or it's tombstoned.
fn page_record(pm: &PageManager, page: &SlottedPageRef, slot: u16) -> Result<Option<Aligned>> {
    if slot >= page.slot_count() || page.is_tombstone(slot) {
        return Ok(None);
    }
    let stored = page.read_record(slot)?;
    compression::decode_record(&overflow::load(pm, stored)?).map(Some)
}

/// Catalog entry `slot` parsed with `parse`; empty when the slot is missing (older files), empty
/// or unreadable. For the definitions whose structures rebuild from the records either way
/// (vector, fulltext, point and text indexes), and the planner statistics.
fn catalog_entry<T: Default>(
    pm: &PageManager,
    catalog_page: &SlottedPageRef,
    slot: u16,
    parse: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    Ok(match page_record(pm, catalog_page, slot)? {
        Some(data) if !data.is_empty() => parse(&data).unwrap_or_default(),
        _ => T::default(),
    })
}

/// Graph + LocationIndex from a loaded PageManager, in three phases (as
/// `Database::open`):
///
/// 1. **Collect** raw compressed records page by page.
/// 2. **Decompress + deserialize** in parallel (rayon).
/// 3. **Construct** the Graph straight from phase 2, without an intermediate
///    `Vec<Edge>` (about half the peak memory of going through `from_parts`).
pub fn load_graph(pm: PageManager) -> Result<(Graph, LocationIndex, PageManager, Vec<u8>)> {
    let catalog_page_id = pm.header().catalog_page_id;
    let catalog_buf = pm.get_page(catalog_page_id)?;
    let catalog_page = SlottedPageRef::new(catalog_buf.as_ref(), catalog_page_id);
    let catalog_data = page_record(&pm, &catalog_page, 0)?
        .ok_or_else(|| Error::InvalidFormat("the catalog page has no catalog".into()))?;
    let catalog = Catalog::from_bytes(&catalog_data)?;

    // phase 1: collect raw compressed records (sequential copies)
    let mut raw_nodes: Vec<(u64, u16, Vec<u8>)> = Vec::new();
    let mut raw_edges: Vec<(u64, u16, Vec<u8>)> = Vec::new();
    for page_id in 0..pm.total_pages() {
        if page_id % 16 == 0 {
            crate::memory::check_memory("page scan")?;
        }
        let buf = pm.get_page(page_id)?;
        let page = SlottedPageRef::new(buf.as_ref(), page_id);
        match page.page_type() {
            Some(PageType::NodeData) => {
                for slot in 0..page.slot_count() {
                    if page.is_tombstone(slot) {
                        continue;
                    }
                    let record_data = page.read_record(slot)?;
                    raw_nodes.push((
                        page_id,
                        slot,
                        overflow::load(&pm, record_data)?.into_owned(),
                    ));
                }
            }
            Some(PageType::EdgeData) => {
                for slot in 0..page.slot_count() {
                    if page.is_tombstone(slot) {
                        continue;
                    }
                    let record_data = page.read_record(slot)?;
                    raw_edges.push((
                        page_id,
                        slot,
                        overflow::load(&pm, record_data)?.into_owned(),
                    ));
                }
            }
            _ => {}
        }
    }

    crate::memory::check_memory("decompression")?;

    // phase 2: parallel decompress + deserialize
    let mem_check_counter = std::sync::atomic::AtomicUsize::new(0);
    let node_results: Vec<(Node, u64, u16)> = raw_nodes
        .into_par_iter()
        .map(|(page_id, slot, bytes)| {
            if mem_check_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(64)
            {
                crate::memory::check_memory("node decompression")?;
            }
            let decoded = compression::decode_record(&bytes)?;
            let archived = archive::access::<Node>(&decoded, "node record")?;
            let node: Node = archive::deserialize(archived, "node record")?;
            Ok((node, page_id, slot))
        })
        .collect::<Result<Vec<_>>>()?;

    mem_check_counter.store(0, std::sync::atomic::Ordering::Relaxed);
    let edge_results: Vec<(crate::graph::LoadedEdge, u64, u16)> = raw_edges
        .into_par_iter()
        .map(|(page_id, slot, bytes)| {
            if mem_check_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(64)
            {
                crate::memory::check_memory("edge decompression")?;
            }
            let decoded = compression::decode_record(&bytes)?;
            let archived = archive::access::<Edge>(&decoded, "relationship record")?;
            // property-less edges: topology straight from the archived form, no allocation
            if archived.properties.count == 0 {
                let loaded = crate::graph::LoadedEdge::Propless {
                    id: EdgeId(archived.id.0.into()),
                    edge_type: EdgeTypeId(archived.edge_type.0.into()),
                    source: NodeId(archived.source.0.into()),
                    target: NodeId(archived.target.0.into()),
                };
                Ok((loaded, page_id, slot))
            } else {
                let edge: Edge = archive::deserialize(archived, "relationship record")?;
                Ok((crate::graph::LoadedEdge::Full(edge), page_id, slot))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    crate::memory::check_memory("graph construction")?;

    // catalog slots 1..=8; older files have fewer
    let index_definitions = match page_record(&pm, &catalog_page, 1)? {
        Some(data) => catalog::IndexCatalog::from_bytes(&data)?.definitions,
        None => Vec::new(),
    };
    let stats = catalog_entry(&pm, &catalog_page, 2, |d| {
        catalog::StatsCatalog::from_bytes(d).map(|c| Some(c.stats))
    })?;
    let constraint_definitions = catalog_entry(&pm, &catalog_page, 3, |d| {
        catalog::ConstraintCatalog::from_bytes(d).map(|c| c.definitions)
    })?;
    let edge_index_definitions = catalog_entry(&pm, &catalog_page, 4, |d| {
        catalog::EdgeIndexCatalog::from_bytes(d).map(|c| c.definitions)
    })?
    .into_iter()
    .map(|(name, edge_type, prop)| {
        let prop_opt = if prop.is_empty() { None } else { Some(prop) };
        (name, edge_type, prop_opt)
    })
    .collect();
    let vector_index_definitions = catalog_entry(&pm, &catalog_page, 5, |d| {
        catalog::VectorIndexCatalog::from_bytes(d).map(|c| c.definitions)
    })?;
    let fulltext_index_definitions = catalog_entry(&pm, &catalog_page, 6, |d| {
        catalog::FulltextIndexCatalog::from_bytes(d).map(|c| c.definitions)
    })?;
    let point_index_definitions = catalog_entry(&pm, &catalog_page, 7, |d| {
        catalog::PointIndexCatalog::from_bytes(d).map(|c| c.definitions)
    })?;
    let text_index_definitions = catalog_entry(&pm, &catalog_page, 8, |d| {
        catalog::TextIndexCatalog::from_bytes(d).map(|c| c.definitions)
    })?;

    // metadata page, if present
    let metadata_page_id = pm.header().metadata_page_id;
    let metadata = if metadata_page_id > 0 && metadata_page_id < pm.total_pages() {
        let meta_buf = pm.get_page(metadata_page_id)?;
        let meta_page = SlottedPageRef::new(meta_buf.as_ref(), metadata_page_id);
        if meta_page.page_type() == Some(PageType::Metadata) {
            page_record(&pm, &meta_page, 0)?
                .map(|m| m.to_vec())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let (graph, locations) = Graph::from_loaded(
        node_results,
        edge_results,
        catalog.strings,
        catalog.next_node_id,
        catalog.next_edge_id,
        index_definitions,
        constraint_definitions,
        edge_index_definitions,
        vector_index_definitions,
        stats,
    )?;

    // full-text indexes aren't part of `from_loaded`'s args: install from descriptors after
    // construction (nodes are loaded, so postings rebuild)
    let mut graph = graph;
    graph.install_fulltext_index_definitions(&fulltext_index_definitions);
    graph.install_point_index_definitions(&point_index_definitions);
    graph.install_text_index_definitions(&text_index_definitions);

    Ok((graph, locations, pm, metadata))
}

/// Records a file is written from: the live graph (its caller keeps writers out) or a
/// [`Checkpoint`] of a running store.
pub(crate) trait RecordSource {
    /// Calls `node` on every node, then `edge` on every relationship; stops at the first error.
    fn for_each_record(
        &self,
        node: &mut dyn FnMut(&Node) -> Result<()>,
        edge: &mut dyn FnMut(&Edge) -> Result<()>,
    ) -> Result<()>;

    /// Calls `f` on each of the nodes `ids` the source has (an in-place save's changed records).
    fn nodes_by_id(&self, ids: &[NodeId], f: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()>;

    /// [`nodes_by_id`](Self::nodes_by_id) for relationships.
    fn edges_by_id(&self, ids: &[EdgeId], f: &mut dyn FnMut(&Edge) -> Result<()>) -> Result<()>;
}

impl RecordSource for Graph {
    fn for_each_record(
        &self,
        node: &mut dyn FnMut(&Node) -> Result<()>,
        edge: &mut dyn FnMut(&Edge) -> Result<()>,
    ) -> Result<()> {
        Graph::for_each_record(self, node, edge)
    }

    fn nodes_by_id(&self, ids: &[NodeId], f: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()> {
        for &id in ids {
            if let Ok(node) = self.get_node(id) {
                f(node)?;
            }
        }
        Ok(())
    }

    fn edges_by_id(&self, ids: &[EdgeId], f: &mut dyn FnMut(&Edge) -> Result<()>) -> Result<()> {
        for &id in ids {
            if let Ok(edge) = self.get_edge(id) {
                f(&edge)?;
            }
        }
        Ok(())
    }
}

/// Records a full write put in the file.
#[derive(Debug)]
pub(crate) struct Written {
    pub(crate) nodes: usize,
    pub(crate) edges: usize,
}

/// Fills an empty PageManager with a Graph + optional opaque metadata (the
/// server's auth catalog). Pages end up dirty in page_id order: native callers
/// then `pm.flush()`, WASM callers `pm.to_bytes()` for OPFS / fetch / download.
///
/// Same bytes as `Database::save` (catalog layout, record encoding,
/// compression): WASM-written tvdb files load natively and vice versa.
pub fn write_graph(pm: &mut PageManager, graph: &Graph, metadata: &[u8]) -> Result<()> {
    write_records(pm, graph, &graph.catalog_parts(), metadata, None).map(|_| ())
}

/// [`write_graph`] for a checkpoint: the store as of its commit, written while reads and commits
/// carry on. Returns the `(nodes, relationships)` written.
pub fn write_checkpoint(
    pm: &mut PageManager,
    checkpoint: &Checkpoint<'_>,
    metadata: &[u8],
) -> Result<(usize, usize)> {
    let written = write_records(pm, checkpoint, checkpoint.catalog(), metadata, None)?;
    Ok((written.nodes, written.edges))
}

/// Writes every record of `source` to an empty PageManager one at a time, then the catalog page
/// (`catalog`) and the metadata page. `locations` gets each record's place.
pub(crate) fn write_records(
    pm: &mut PageManager,
    source: &dyn RecordSource,
    catalog: &CatalogParts,
    metadata: &[u8],
    locations: Option<&mut LocationIndex>,
) -> Result<Written> {
    // the two callbacks share the page manager and the location index
    let shared = std::cell::RefCell::new((&mut *pm, locations));
    let (mut nodes, mut edges) = (0, 0);
    let (mut node_page, mut edge_page) = (None, None);
    source.for_each_record(
        &mut |node| {
            let encoded = encode_node(node)?;
            let (pm, locations) = &mut *shared.borrow_mut();
            let encoded = overflow::store(pm, encoded)?;
            let loc =
                insert_record_to_page_append(pm, &encoded, PageType::NodeData, &mut node_page)?;
            if let Some(l) = locations {
                l.set_node(node.id, loc);
            }
            nodes += 1;
            Ok(())
        },
        &mut |edge| {
            let encoded = encode_edge(edge)?;
            let (pm, locations) = &mut *shared.borrow_mut();
            let encoded = overflow::store(pm, encoded)?;
            let loc =
                insert_record_to_page_append(pm, &encoded, PageType::EdgeData, &mut edge_page)?;
            if let Some(l) = locations {
                l.set_edge(edge.id, loc);
            }
            edges += 1;
            Ok(())
        },
    )?;
    write_catalog_page(pm, catalog)?;
    write_metadata_page(pm, metadata)?;
    Ok(Written { nodes, edges })
}

/// One node as stored: archived, then compressed. One larger than a page goes in its slot through
/// [`overflow::store`].
pub(crate) fn encode_node(node: &Node) -> Result<Vec<u8>> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(node)
        .map_err(|e| Error::Serialization(e.to_string()))?;
    Ok(compression::encode_record(&bytes))
}

/// [`encode_node`] for a relationship.
pub(crate) fn encode_edge(edge: &Edge) -> Result<Vec<u8>> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(edge)
        .map_err(|e| Error::Serialization(e.to_string()))?;
    Ok(compression::encode_record(&bytes))
}

/// The catalog page's records in slot order: names and id counters, property indexes,
/// statistics (empty without), constraints, relationship indexes, then vector, fulltext, point
/// and text indexes.
fn catalog_records(parts: &CatalogParts) -> Result<Vec<Vec<u8>>> {
    let edge_index_definitions = parts
        .edge_index_definitions
        .iter()
        .map(|(name, edge_type, key)| {
            (
                name.clone(),
                edge_type.clone(),
                key.clone().unwrap_or_default(),
            )
        })
        .collect();
    // statistics are planner hints: unencodable ones are left out, not a failed save
    let stats = match &parts.stats {
        Some(stats) => catalog::StatsCatalog {
            stats: stats.clone(),
        }
        .to_bytes()
        .unwrap_or_default(),
        None => Vec::new(),
    };
    let raw = [
        Catalog {
            strings: parts.strings.clone(),
            next_node_id: parts.next_node_id,
            next_edge_id: parts.next_edge_id,
        }
        .to_bytes()?,
        catalog::IndexCatalog {
            definitions: parts.index_definitions.clone(),
        }
        .to_bytes()?,
        stats,
        catalog::ConstraintCatalog {
            definitions: parts.constraint_definitions.clone(),
        }
        .to_bytes()?,
        catalog::EdgeIndexCatalog {
            definitions: edge_index_definitions,
        }
        .to_bytes()?,
        catalog::VectorIndexCatalog {
            definitions: parts.vector_index_definitions.clone(),
        }
        .to_bytes()?,
        catalog::FulltextIndexCatalog {
            definitions: parts.fulltext_index_definitions.clone(),
        }
        .to_bytes()?,
        catalog::PointIndexCatalog {
            definitions: parts.point_index_definitions.clone(),
        }
        .to_bytes()?,
        catalog::TextIndexCatalog {
            definitions: parts.text_index_definitions.clone(),
        }
        .to_bytes()?,
    ];
    Ok(raw.iter().map(|r| compression::encode_record(r)).collect())
}

/// Writes the catalog page and points the header at it; returns its id. Entries go in their slots
/// while the page holds them all; past that the largest continue on overflow pages, so every
/// entry keeps its slot whatever the size of the schema.
pub(crate) fn write_catalog_page(pm: &mut PageManager, parts: &CatalogParts) -> Result<u64> {
    let mut records = catalog_records(parts)?;
    let size =
        |records: &[Vec<u8>]| -> usize { records.iter().map(|r| r.len() + SLOT_ENTRY_SIZE).sum() };
    while size(&records) > PAGE_DATA_CAPACITY {
        // the largest entry still in the page; a stub is smaller than any entry worth moving
        let Some((largest, _)) = records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.first() != Some(&compression::RECORD_FLAG_OVERFLOW))
            .max_by_key(|(_, r)| r.len())
        else {
            return Err(Error::InvalidFormat(
                "catalog stubs don't fit the catalog page".into(),
            ));
        };
        let record = std::mem::take(&mut records[largest]);
        records[largest] = overflow::spill(pm, record)?;
    }
    let page_id = pm.allocate(PageType::Catalog);
    let buf = pm.get_page_mut(page_id)?;
    let mut page = SlottedPage::new(buf.as_mut(), page_id);
    for record in &records {
        page.insert_record(record)?;
    }
    pm.header_mut().catalog_page_id = page_id;
    Ok(page_id)
}

/// Writes `metadata` (the server's user catalog, opaque here) to a fresh page referenced from the
/// header; nothing when empty.
pub(crate) fn write_metadata_page(pm: &mut PageManager, metadata: &[u8]) -> Result<()> {
    if metadata.is_empty() {
        return Ok(());
    }
    let record = overflow::store(pm, compression::encode_record(metadata))?;
    let page_id = pm.allocate(PageType::Metadata);
    let buf = pm.get_page_mut(page_id)?;
    let mut page = SlottedPage::new(buf.as_mut(), page_id);
    page.insert_record(&record)?;
    pm.header_mut().metadata_page_id = page_id;
    Ok(())
}

/// Inserts an encoded record: hint page -> fresh page, no scan (every earlier page of a file being
/// written start to end is full).
pub(crate) fn insert_record_to_page_append(
    pm: &mut PageManager,
    encoded: &[u8],
    page_type: PageType,
    hint: &mut Option<u64>,
) -> Result<RecordLocation> {
    let needed = encoded.len() + SLOT_ENTRY_SIZE;

    if let Some(hint_page_id) = *hint
        && let Ok(buf) = pm.get_page_mut(hint_page_id)
    {
        let mut page = SlottedPage::new(buf.as_mut(), hint_page_id);
        if page.free_space() >= needed {
            let slot_idx = page.insert_record(encoded)?;
            return Ok(RecordLocation {
                page_id: hint_page_id,
                slot_idx,
            });
        }
    }

    let new_page_id = pm.allocate(page_type);
    let buf = pm.get_page_mut(new_page_id)?;
    let mut page = SlottedPage::new(buf.as_mut(), new_page_id);
    let slot_idx = page.insert_record(encoded)?;
    *hint = Some(new_page_id);
    Ok(RecordLocation {
        page_id: new_page_id,
        slot_idx,
    })
}
