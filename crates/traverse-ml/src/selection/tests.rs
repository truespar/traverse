use std::io::ErrorKind;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use super::search::plan_trials;
use super::*;
use crate::cancel::CancellationToken;
use crate::encoder::GraphInputs;
use crate::pipeline::{ModelChoice, ModelKind, TaskType};
use crate::sampling::Fanout;

type BA = burn::backend::Autodiff<burn::backend::NdArray>;
type BI = burn::backend::NdArray;

/// Two 20-node communities, dense inside, one bridge; features are noise, so the label has to
/// come from the structure.
fn two_communities() -> (NodeData, Vec<i32>) {
    let mut edges = Vec::new();
    for c in 0..2u32 {
        for i in 0..20u32 {
            for j in (i + 1)..20 {
                if (i * 7 + j * 3) % 4 != 0 {
                    edges.push((c * 20 + i, c * 20 + j));
                }
            }
        }
    }
    edges.push((0, 20));
    let rel = vec![0u16; edges.len()];
    let inputs = GraphInputs::from_edges(40, &edges, &rel, 1);
    let mut rng = StdRng::seed_from_u64(3);
    let features: Vec<f32> = (0..40 * 4).map(|_| rng.random_range(-1.0..1.0)).collect();
    let labels: Vec<i32> = (0..40).map(|i| i / 20).collect();
    (
        NodeData {
            inputs,
            features,
            feat_dim: 4,
        },
        labels,
    )
}

fn quick(task: TaskType) -> SelectionConfig {
    SelectionConfig {
        epochs: 60,
        patience: None,
        ..SelectionConfig::defaults(task)
    }
}

#[test]
fn link_metrics_match_hand_computed_values() {
    let m = link_metrics(&[0.9, 0.8, 0.3], &[0.7, 0.2]);
    // pos > neg in 5 of 6 pairs
    assert!((m[&Metric::RocAuc] - 5.0 / 6.0).abs() < 1e-9);
    // ranking: 0.9+ 0.8+ 0.7- 0.3+ 0.2-: AP = (1 + 1 + 3/4) / 3
    assert!((m[&Metric::Aucpr] - (2.75 / 3.0)).abs() < 1e-9);
    let ties = link_metrics(&[0.5], &[0.5]);
    assert!((ties[&Metric::RocAuc] - 0.5).abs() < 1e-9);
}

#[test]
fn concrete_candidates_come_first_then_samples() {
    let cfg = SelectionConfig {
        max_trials: 5,
        ..SelectionConfig::defaults(TaskType::NodeClassification)
    };
    let ranged = CandidateSpec {
        kind: ModelKind::Gat,
        hidden: ParamSpace::Range(8, 16),
        heads: ParamSpace::Range(1, 4),
        learning_rate: ParamSpace::Range(1e-3, 1e-1),
    };
    let plans = plan_trials(
        &[CandidateSpec::fixed(ModelKind::Gcn, 16, 0.01), ranged],
        &cfg,
    )
    .unwrap();
    assert_eq!(plans.len(), 5);
    assert_eq!(plans[0].choice, ModelChoice::Gcn { hidden: 16 });
    for p in &plans[1..] {
        let ModelChoice::Gat { hidden, heads } = p.choice else {
            panic!("expected GAT, got {:?}", p.choice)
        };
        assert!((8..=16).contains(&hidden) && (1..=4).contains(&heads));
        assert!((1e-3..=1e-1).contains(&p.learning_rate));
    }
}

#[test]
fn classification_selects_and_predicts() {
    let (data, labels) = two_communities();
    let device = Default::default();
    let candidates = [
        CandidateSpec::fixed(ModelKind::Gcn, 16, 0.02),
        CandidateSpec::fixed(ModelKind::GraphSage, 16, 0.02),
    ];
    let r = train_node_classification_selected::<BA>(
        &data,
        &labels,
        2,
        &candidates,
        &quick(TaskType::NodeClassification),
        &device,
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(r.trials.len(), 2);
    assert_eq!(r.train_size + r.test_size, 40);
    assert!(r.test[&Metric::Accuracy] >= 0.8, "test {:?}", r.test);
    let preds = predict_classes::<BI>(&r.model, &data.inputs, &data.features, 4, &device).unwrap();
    let correct = preds
        .iter()
        .zip(&labels)
        .filter(|((c, _), l)| c == *l)
        .count();
    assert!(correct >= 32, "{correct}/40 after reload");
}

#[test]
fn successive_halving_ends_on_one_full_budget_trial() {
    let (data, labels) = two_communities();
    let cfg = SelectionConfig {
        max_trials: 6,
        strategy: SearchStrategy::SuccessiveHalving {
            eta: 3,
            min_epochs: 10,
        },
        ..quick(TaskType::NodeClassification)
    };
    let candidates = [CandidateSpec {
        kind: ModelKind::Gcn,
        hidden: ParamSpace::Range(8, 32),
        heads: ParamSpace::Fixed(1),
        learning_rate: ParamSpace::Range(5e-3, 5e-2),
    }];
    let r = train_node_classification_selected::<BA>(
        &data,
        &labels,
        2,
        &candidates,
        &cfg,
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    // 6 at 10 epochs, 2 at 30, 1 at 60
    assert_eq!(r.trials.len(), 9);
    assert_eq!(r.best().epochs, 60);
}

#[test]
fn one_trial_on_a_tiny_training_set_trains_without_folds() {
    let (data, full) = two_communities();
    // two labelled nodes per class: too few for 3 folds after the test holdout
    let labels: Vec<i32> = full
        .iter()
        .enumerate()
        .map(|(i, &l)| if i % 20 < 2 { l } else { -1 })
        .collect();
    let cfg = SelectionConfig {
        strategy: SearchStrategy::SuccessiveHalving {
            eta: 3,
            min_epochs: 5,
        },
        ..quick(TaskType::NodeClassification)
    };
    let r = train_node_classification_selected::<BA>(
        &data,
        &labels,
        2,
        &[CandidateSpec::fixed(ModelKind::Gcn, 8, 0.02)],
        &cfg,
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(r.trials.len(), 1);
    assert!(r.best().validation.is_empty());
    assert_eq!(
        r.best().epochs,
        cfg.epochs,
        "a lone trial gets the full budget"
    );
    // two candidates can't be compared without folds
    let err = train_node_classification_selected::<BA>(
        &data,
        &labels,
        2,
        &[
            CandidateSpec::fixed(ModelKind::Gcn, 8, 0.02),
            CandidateSpec::fixed(ModelKind::GraphSage, 8, 0.02),
        ],
        &cfg,
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("validation folds"), "{err}");
}

#[test]
fn regression_recovers_a_structural_target() {
    let (data, labels) = two_communities();
    // community 0 -> 10.0, community 1 -> 30.0, with a gap to learn
    let targets: Vec<f32> = labels.iter().map(|&l| 10.0 + 20.0 * l as f32).collect();
    let r = train_node_regression_selected::<BA>(
        &data,
        &targets,
        &[CandidateSpec::fixed(ModelKind::Gcn, 16, 0.02)],
        &quick(TaskType::NodeRegression),
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    assert!(
        r.test[&Metric::RootMeanSquaredError] < 6.0,
        "test {:?}",
        r.test
    );
    let preds = predict_values::<BI>(
        &r.model,
        &data.inputs,
        &data.features,
        4,
        &Default::default(),
    )
    .unwrap();
    assert!((preds[5] - 10.0).abs() < 6.0 && (preds[35] - 30.0).abs() < 6.0);
}

#[test]
fn link_prediction_beats_chance() {
    let (data, _) = two_communities();
    // learn to predict the community edges from features + context
    let mut positives = Vec::new();
    for u in 0..data.inputs.num_nodes() {
        for (v, _) in data.inputs.sym.neighbors(u) {
            if (u as u32) < v {
                positives.push((u as u32, v));
            }
        }
    }
    let link = LinkData {
        num_nodes: 40,
        features: data.features.clone(),
        feat_dim: 4,
        context: Vec::new(),
        num_relations: 1,
        target_relation: 0,
        positives,
        source_nodes: None,
        target_nodes: None,
    };
    let r = train_link_prediction_selected::<BA>(
        &link,
        &[CandidateSpec::fixed(ModelKind::Gcn, 16, 0.02)],
        &SelectionConfig {
            embedding_dim: 16,
            ..quick(TaskType::LinkPrediction)
        },
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    assert!(r.test[&Metric::RocAuc] > 0.75, "test {:?}", r.test);
}

fn mini_batch(task: TaskType) -> SelectionConfig {
    SelectionConfig {
        batching: Batching::MiniBatch {
            batch_size: 8,
            fanouts: [Fanout::Sample(4), Fanout::Sample(3)],
        },
        ..quick(task)
    }
}

#[test]
fn auto_batching_follows_the_graph_size() {
    let small = Batching::Auto.resolve(1_000, 64);
    assert_eq!(small, Batching::FullGraph);
    let big = Batching::Auto.resolve(FULL_GRAPH_VALUES / 64 + 1, 64);
    assert_eq!(
        big,
        Batching::MiniBatch {
            batch_size: DEFAULT_BATCH_SIZE,
            fanouts: DEFAULT_FANOUTS
        }
    );
    // an explicit choice stands
    assert_eq!(
        Batching::FullGraph.resolve(usize::MAX, 64),
        Batching::FullGraph
    );
}

#[test]
fn mini_batch_classification_selects_and_predicts() {
    let (data, labels) = two_communities();
    let device = Default::default();
    for kind in [
        ModelKind::Gcn,
        ModelKind::GraphSage,
        ModelKind::RGcn,
        ModelKind::Gat,
    ] {
        let r = train_node_classification_selected::<BA>(
            &data,
            &labels,
            2,
            &[CandidateSpec::fixed(kind, 16, 0.02)],
            &mini_batch(TaskType::NodeClassification),
            &device,
            &CancellationToken::none(),
            &|_, _| {},
        )
        .unwrap();
        assert!(
            matches!(r.batching, Batching::MiniBatch { batch_size: 8, .. }),
            "{kind:?}"
        );
        assert!(
            r.test[&Metric::Accuracy] >= 0.8,
            "{kind:?}: test {:?}",
            r.test
        );
        let preds =
            predict_classes::<BI>(&r.model, &data.inputs, &data.features, 4, &device).unwrap();
        let correct = preds
            .iter()
            .zip(&labels)
            .filter(|((c, _), l)| c == *l)
            .count();
        assert!(correct >= 32, "{kind:?}: {correct}/40");
        assert!(r.model.metadata.extra["batching"].contains("MiniBatch"));
    }
}

#[test]
fn mini_batch_regression_and_link_prediction_learn() {
    let (data, labels) = two_communities();
    let targets: Vec<f32> = labels.iter().map(|&l| 10.0 + 20.0 * l as f32).collect();
    let r = train_node_regression_selected::<BA>(
        &data,
        &targets,
        &[CandidateSpec::fixed(ModelKind::GraphSage, 16, 0.02)],
        &mini_batch(TaskType::NodeRegression),
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    assert!(
        r.test[&Metric::RootMeanSquaredError] < 6.0,
        "test {:?}",
        r.test
    );

    let mut positives = Vec::new();
    for u in 0..data.inputs.num_nodes() {
        for (v, _) in data.inputs.sym.neighbors(u) {
            if (u as u32) < v {
                positives.push((u as u32, v));
            }
        }
    }
    let link = LinkData {
        num_nodes: 40,
        features: data.features.clone(),
        feat_dim: 4,
        context: Vec::new(),
        num_relations: 1,
        target_relation: 0,
        positives,
        source_nodes: None,
        target_nodes: None,
    };
    let r = train_link_prediction_selected::<BA>(
        &link,
        &[CandidateSpec::fixed(ModelKind::Gcn, 16, 0.02)],
        &SelectionConfig {
            embedding_dim: 16,
            ..mini_batch(TaskType::LinkPrediction)
        },
        &Default::default(),
        &CancellationToken::none(),
        &|_, _| {},
    )
    .unwrap();
    assert!(r.test[&Metric::RocAuc] > 0.75, "test {:?}", r.test);
}

#[test]
fn a_cancelled_fit_stops() {
    let (data, labels) = two_communities();
    let cancel = CancellationToken::new();
    cancel.cancel();
    for cfg in [
        quick(TaskType::NodeClassification),
        mini_batch(TaskType::NodeClassification),
    ] {
        let err = train_node_classification_selected::<BA>(
            &data,
            &labels,
            2,
            &[CandidateSpec::fixed(ModelKind::Gcn, 8, 0.02)],
            &cfg,
            &Default::default(),
            &cancel,
            &|_, _| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Interrupted);
    }
}
