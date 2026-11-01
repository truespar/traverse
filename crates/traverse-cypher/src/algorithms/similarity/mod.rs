//! Similarity and topological link prediction (Jaccard, Cosine, kNN, Adamic-Adar, Common Neighbors,
//! Preferential Attachment, Resource Allocation, Same Community, Total Neighbors). All output
//! `OutputKind::NodePair`.

pub(crate) mod helpers;

pub mod cosine_similarity;
pub mod knn;
pub mod knn_property;
pub mod node_similarity;
pub mod topological_lp;

/// Registers every similarity and link-prediction algorithm in the current thread's registry.
pub fn register() {
    node_similarity::register();
    cosine_similarity::register();
    knn::register();
    knn_property::register();
    topological_lp::register();
}
