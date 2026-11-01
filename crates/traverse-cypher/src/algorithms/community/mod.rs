//! Community detection and quality: WCC, SCC, Louvain, Leiden, Label Propagation, K-Core, Triangle
//! Count + LCC, Modularity, Conductance.

pub mod conductance;
pub mod k_core;
pub mod label_propagation;
pub mod leiden;
pub mod louvain;
pub mod modularity;
pub mod scc;
pub mod triangle_count;
pub mod wcc;

/// Registers every community algorithm in the current thread's registry.
pub fn register() {
    wcc::register();
    triangle_count::register();
    scc::register();
    label_propagation::register();
    k_core::register();
    louvain::register();
    leiden::register();
    modularity::register();
    conductance::register();
}
