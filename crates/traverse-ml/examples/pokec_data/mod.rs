//! Pokec loader for the examples: the SNAP Slovak social network with demographic node attributes,
//! a standard node-classification benchmark.

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use traverse_ml::graph::CsrGraph;
use traverse_ml::loader::ClassificationDataset;

/// Loads the Pokec-small dataset (10k nodes, ~122k edges) from CSVs in the
/// `:ID,...,:LABEL` format of the Cypher import tool, by default:
///
/// ```text
/// benchmarks/data/dataset_cache/pokec_small_nodes.csv
/// benchmarks/data/dataset_cache/pokec_small_edges.csv
/// ```
///
/// Task: predict `gender` (binary, man=0, woman=1). Nodes with `gender=""`
/// stay in the graph but out of train/val/test (`labels[i] = -1`).
///
/// Features (`feat_dim=2`): `[completion_percentage / 100, age / 100]`, both
/// roughly `[0, 1]`.
///
/// Splits: 60% train, 20% val, 20% test over labeled nodes, deterministic per
/// `seed`.
pub fn load_pokec_small(
    nodes_csv: &Path,
    edges_csv: &Path,
    seed: u64,
) -> std::io::Result<ClassificationDataset> {
    // -- 1. nodes: pokec_id -> dense index map + per-node features/labels
    let file = std::fs::File::open(nodes_csv)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // skip the header
    let _ = lines.next();

    let mut id_map: HashMap<u32, u32> = HashMap::new();
    let mut features: Vec<f32> = Vec::new();
    let mut labels: Vec<i32> = Vec::new();
    let feat_dim = 2usize;

    for line in lines {
        let line = line?;
        // format: :ID,completion_percentage,gender,age,:LABEL
        let mut parts = line.split(',');
        let pokec_id: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad node id in: {line}"),
            )
        })?;
        let completion: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let gender = parts.next().unwrap_or("");
        let age: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);

        let dense_idx = features.len() / feat_dim;
        id_map.insert(pokec_id, dense_idx as u32);
        features.push(completion / 100.0);
        features.push(age / 100.0);
        labels.push(match gender {
            "man" => 0,
            "woman" => 1,
            _ => -1,
        });
    }

    let num_nodes = labels.len();

    // -- 2. edges: pokec_id pairs -> dense edge list
    let file = std::fs::File::open(edges_csv)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let _ = lines.next(); // header

    let mut edges: Vec<(u32, u32, f32)> = Vec::new();
    for line in lines {
        let line = line?;
        let mut parts = line.split(',');
        let src_pokec: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad edge src in: {line}"),
            )
        })?;
        let dst_pokec: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad edge dst in: {line}"),
            )
        })?;
        match (id_map.get(&src_pokec), id_map.get(&dst_pokec)) {
            (Some(&s), Some(&d)) => edges.push((s, d, 1.0)),
            _ => continue, // drop edges to unknown nodes
        }
    }

    // -- 3. normalized adjacency
    let graph = CsrGraph::from_edges(num_nodes, &edges)
        .add_self_loops()
        .symmetric_normalize();

    // -- 4. train/val/test split over labeled nodes only
    let mut labelled_indices: Vec<u32> = (0..num_nodes as u32)
        .filter(|&i| labels[i as usize] >= 0)
        .collect();
    let mut rng = StdRng::seed_from_u64(seed);
    labelled_indices.shuffle(&mut rng);

    let n_lab = labelled_indices.len();
    let n_train = (n_lab as f32 * 0.6) as usize;
    let n_val = (n_lab as f32 * 0.2) as usize;

    let mut train_mask = vec![false; num_nodes];
    let mut val_mask = vec![false; num_nodes];
    let mut test_mask = vec![false; num_nodes];
    for &idx in &labelled_indices[..n_train] {
        train_mask[idx as usize] = true;
    }
    for &idx in &labelled_indices[n_train..n_train + n_val] {
        val_mask[idx as usize] = true;
    }
    for &idx in &labelled_indices[n_train + n_val..] {
        test_mask[idx as usize] = true;
    }

    Ok(ClassificationDataset {
        graph,
        features,
        feat_dim,
        labels,
        train_mask,
        val_mask,
        test_mask,
        num_classes: 2,
    })
}
