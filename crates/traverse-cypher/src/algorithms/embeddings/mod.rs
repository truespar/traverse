//! Node embeddings (FastRP, Node2Vec, HashGNN, GraphSAGE). All output `OutputKind::NodeVector { dim
//! }`, written back as `List<Float>` node properties.

pub mod fastrp;
pub mod graphsage;
pub mod hashgnn;
pub mod node2vec;

/// Registers every embedding algorithm in the current thread's registry.
pub fn register() {
    fastrp::register();
    hashgnn::register();
    node2vec::register();
    graphsage::register();
}
