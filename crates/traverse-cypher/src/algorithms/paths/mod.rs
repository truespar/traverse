//! Path finding: BFS, DFS, Dijkstra, Delta-Stepping, A*, Yen's, Bellman-Ford, all-pairs shortest
//! paths, spanning tree, Steiner tree, random walk.

pub mod all_pairs;
pub mod astar;
pub mod bellman_ford;
pub mod bfs;
pub mod delta_stepping;
pub mod dfs;
pub mod dijkstra;
pub mod random_walk;
pub mod spanning_tree;
pub mod steiner_tree;
pub mod yens;

/// Registers every path-finding algorithm in the current thread's registry.
pub fn register() {
    bfs::register();
    dfs::register();
    dijkstra::register();
    astar::register();
    random_walk::register();
    bellman_ford::register();
    yens::register();
    all_pairs::register();
    delta_stepping::register();
    spanning_tree::register();
    steiner_tree::register();
}
