//! Traverse Core: property graph data structures and in-memory storage.
//!
//! Building blocks of the embedded graph database: nodes, edges, labels,
//! properties, and [`graph::Graph`] tying them together with CRUD and traversal.
//!
//! # Quick Start
//!
//! ```
//! use traverse_core::graph::Graph;
//! use traverse_core::graph::property::PropertyValue;
//! use traverse_core::graph::types::Direction;
//! use std::collections::BTreeMap;
//!
//! let mut g = Graph::new();
//!
//! let mut props = BTreeMap::new();
//! props.insert("name".to_string(), PropertyValue::String("Alice".into()));
//! let alice = g.create_node(&["Person"], props);
//!
//! let bob = g.create_node(&["Person"], BTreeMap::new());
//! let _edge = g.create_edge(alice, bob, "KNOWS", BTreeMap::new()).unwrap();
//!
//! let friends = g.neighbors(alice, Direction::Outgoing, None).unwrap();
//! assert_eq!(friends.len(), 1);
//! ```

#![warn(missing_docs)]

// file-bound (`Database`), so not on wasm32
#[cfg(not(target_arch = "wasm32"))]
pub mod embedded;
/// Error types for graph operations.
pub mod error;
pub mod graph;
/// String interning for labels and edge types.
pub mod interner;
pub mod memory;
pub mod storage;
pub mod sync;
pub mod tx;

// paths the GraphView forwarding macros expand to in other crates
#[doc(hidden)]
pub use rustc_hash as __rustc_hash;
