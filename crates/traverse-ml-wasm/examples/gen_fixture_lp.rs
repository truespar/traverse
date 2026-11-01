//! Generate the WASM smoke-test fixture for link prediction
//! (`tests/fixtures/tiny_lp.tvmodel`).
//!
//! Trains a 2-layer GCN encoder + dot-product decoder on a tiny
//! 6-node 2-cluster graph. After training, intra-cluster pairs
//! should score higher than inter-cluster pairs - this is what
//! the WASM smoke test asserts.
//!
//! Run from the workspace root:
//!
//! ```sh
//! cargo run --example gen_fixture_lp -p traverse-ml-wasm
//! ```

use burn::module::Module;
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::prelude::ElementConversion;
use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
use burn::tensor::{Int, Tensor, backend::AutodiffBackend};
use std::path::PathBuf;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::graph::CsrGraph;
use traverse_ml::model::{Model, ModelCatalog, ModelMetadata};

type B = burn::backend::Autodiff<burn::backend::NdArray>;

fn dot_score<BA: AutodiffBackend>(
    z: Tensor<BA, 2>,
    edges: &[(u32, u32)],
    device: &BA::Device,
) -> Tensor<BA, 1> {
    let n_edges = edges.len();
    let emb_dim = z.dims()[1];
    let src: Vec<i64> = edges.iter().map(|(u, _)| *u as i64).collect();
    let dst: Vec<i64> = edges.iter().map(|(_, v)| *v as i64).collect();
    let src_t = Tensor::<BA, 1, Int>::from_ints(src.as_slice(), device);
    let dst_t = Tensor::<BA, 1, Int>::from_ints(dst.as_slice(), device);
    let src_2d = src_t.unsqueeze_dim::<2>(1).expand([n_edges, emb_dim]);
    let dst_2d = dst_t.unsqueeze_dim::<2>(1).expand([n_edges, emb_dim]);
    let z_src = z.clone().gather(0, src_2d);
    let z_dst = z.gather(0, dst_2d);
    (z_src * z_dst).sum_dim(1).squeeze::<1>()
}

fn bce<BA: AutodiffBackend>(logits: Tensor<BA, 1>, positive: bool) -> Tensor<BA, 1> {
    // BCE(target=1, x) = softplus(-x); BCE(target=0, x) = softplus(x).
    let z = if positive { -logits } else { logits };
    let exp_z = z.exp();
    (exp_z + 1.0).log().mean()
}

fn main() -> std::io::Result<()> {
    let device = burn::backend::ndarray::NdArrayDevice::default();

    // 6 nodes, 2 clusters: {0,1,2} and {3,4,5} fully connected
    // intra-cluster, plus a (2,3) bridge edge. 7 undirected edges.
    let edges = vec![
        (0u32, 1u32, 1.0f32),
        (0, 2, 1.0),
        (1, 2, 1.0),
        (3, 4, 1.0),
        (3, 5, 1.0),
        (4, 5, 1.0),
        (2, 3, 1.0), // bridge
    ];
    let num_nodes = 6usize;
    let raw_graph = CsrGraph::from_edges(num_nodes, &edges);

    // Block-correlated features.
    let features: Vec<f32> = vec![
        0.0, 0.1, // 0 (block 0)
        0.0, 0.0, // 1 (block 0)
        0.1, 0.0, // 2 (block 0)
        1.0, 0.9, // 3 (block 1)
        1.0, 1.0, // 4 (block 1)
        0.9, 1.0, // 5 (block 1)
    ];

    let x: Tensor<B, 2> =
        Tensor::<B, 1>::from_floats(features.as_slice(), &device).reshape([num_nodes, 2]);

    // Message-passing graph for training: train_graph + self_loops +
    // sym_normalize, same recipe the trainer uses internally.
    let sym_graph = raw_graph.clone().add_self_loops().symmetric_normalize();

    let emb_dim = 8;
    let mut model: Gcn<B> = GcnConfig::new(2, 16, emb_dim).init(&device);
    let mut optim = AdamConfig::new().init();

    // Positives: the 7 edges above. Negatives: random non-edges
    // sampled fresh each epoch.
    let pos_edges: Vec<(u32, u32)> = edges.iter().map(|(s, d, _)| (*s, *d)).collect();

    let mut last_loss = f32::NAN;
    let mut rng_state: u32 = 0xDEADBEEF;
    for _ in 1..=200 {
        let z = model.forward(x.clone(), &sym_graph);
        // Sample 7 random non-edges (good enough for tiny graph).
        let mut neg: Vec<(u32, u32)> = Vec::with_capacity(7);
        while neg.len() < 7 {
            // xorshift32 - deterministic across runs.
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 17;
            rng_state ^= rng_state << 5;
            let u = rng_state % num_nodes as u32;
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 17;
            rng_state ^= rng_state << 5;
            let v = rng_state % num_nodes as u32;
            if u != v {
                neg.push((u, v));
            }
        }
        let pos_scores = dot_score::<B>(z.clone(), &pos_edges, &device);
        let neg_scores = dot_score::<B>(z, &neg, &device);
        let loss = bce::<B>(pos_scores, true) + bce::<B>(neg_scores, false);
        last_loss = loss.clone().into_scalar().elem();
        let grads = loss.backward();
        let grads = GradientsParams::from_grads(grads, &model);
        model = optim.step(1e-2, model, grads);
    }
    eprintln!("final LP training loss: {last_loss:.4}");

    let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();
    let weights = recorder.record(model.into_record(), ()).expect("recorder");

    let mut meta = ModelMetadata::now();
    meta.feature_dim = 2;
    meta.num_classes = emb_dim; // For LP, "num_classes" stores embedding dim.
    meta.train_graph_nodes = num_nodes;
    meta.train_graph_edges = raw_graph.num_edges(); // RAW edges (LP fingerprint matches train_graph, no self-loops).
    meta.training_metrics.insert("final_loss".into(), last_loss);
    meta.extra
        .insert("architecture".into(), "gcn-2layer-h16".into());
    meta.extra
        .insert("embedding_dim".into(), emb_dim.to_string());
    meta.extra
        .insert("fixture".into(), "wasm_predict_lp_smoke".into());
    meta.extra.insert("task".into(), "link_prediction".into());
    meta.set_schema_fingerprint();

    let saved = Model {
        kind: "gcn-2layer".into(),
        metadata: meta,
        weights,
    };
    let fixture_dir: PathBuf = PathBuf::from("crates/traverse-ml-wasm/tests/fixtures");
    let mut catalog = ModelCatalog::new(fixture_dir.clone());
    catalog.insert("tiny_lp", saved);
    catalog.save_to_disk()?;
    eprintln!("wrote {}", fixture_dir.join("tiny_lp.tvmodel").display());
    eprintln!(
        "num_nodes = {}, raw num_edges = {}",
        num_nodes,
        raw_graph.num_edges()
    );
    Ok(())
}
