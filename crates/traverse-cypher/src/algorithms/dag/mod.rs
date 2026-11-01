//! DAG algorithms (Topological Sort, Longest Path). Both reject cyclic input (Kahn's algorithm
//! leaves a cycle unemitted).

pub mod longest_path;
pub mod topological_sort;

/// Registers the DAG algorithms in the current thread's registry.
pub fn register() {
    topological_sort::register();
    longest_path::register();
}
