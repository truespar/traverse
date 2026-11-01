//! Centrality: Degree, PageRank, ArticleRank, Eigenvector, Betweenness, Closeness, Harmonic, HITS,
//! CELF.

pub mod article_rank;
pub mod betweenness;
pub mod celf;
pub mod closeness;
pub mod degree;
pub mod eigenvector;
pub mod harmonic;
pub mod hits;
pub mod pagerank;

/// Registers every centrality algorithm in the current thread's registry.
pub fn register() {
    degree::register();
    pagerank::register();
    article_rank::register();
    eigenvector::register();
    hits::register();
    closeness::register();
    harmonic::register();
    betweenness::register();
    celf::register();
}
