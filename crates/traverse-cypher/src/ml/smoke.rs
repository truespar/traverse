//! End-to-end smoke tests for `traverse.ml.pipeline.*`: full executor (parse -> plan -> run -> bind
//! YIELD), checking rows match the GDS-mirrored signatures.

#![cfg(test)]

use std::sync::{Mutex, MutexGuard, PoisonError};

use traverse_core::graph::Graph;

use crate::execute;
use crate::types::CypherValue;

fn empty_graph() -> Graph {
    Graph::new()
}

/// The pipeline catalog is a process-global static (`registry::CATALOG`) and each test starts with
/// `registry::clear()`; under the parallel harness one test's clear() would hit another mid-flight,
/// so each test holds this lock for its whole body. `into_inner` stops one panicking test from
/// poisoning it for the rest.
static CATALOG_LOCK: Mutex<()> = Mutex::new(());

fn fresh_catalog() -> MutexGuard<'static, ()> {
    let guard = CATALOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    super::registry::clear();
    guard
}

#[test]
fn create_node_classification_pipeline_returns_gds_yield_shape() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    let r = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p1') \
         YIELD name, nodePropertySteps, featureProperties, splitConfig, \
               autoTuningConfig, parameterSpace \
         RETURN name, nodePropertySteps, featureProperties, splitConfig, \
                autoTuningConfig, parameterSpace",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1, "create should yield one row");
    let row = &r.rows[0];
    // name = 'p1'
    assert_eq!(
        row[0],
        CypherValue::String("p1".to_string()),
        "yield[name] = {:?}",
        row[0]
    );
    // nodePropertySteps starts empty
    match &row[1] {
        CypherValue::List(v) => assert!(v.is_empty()),
        other => panic!("nodePropertySteps not a list: {other:?}"),
    }
    // featureProperties starts empty
    match &row[2] {
        CypherValue::List(v) => assert!(v.is_empty()),
        other => panic!("featureProperties not a list: {other:?}"),
    }
    // splitConfig GDS defaults: testFraction=0.3, validationFolds=3
    match &row[3] {
        CypherValue::Map(m) => {
            assert_eq!(
                m.get("testFraction"),
                Some(&CypherValue::Float(0.3)),
                "GDS-default testFraction missing"
            );
            assert_eq!(
                m.get("validationFolds"),
                Some(&CypherValue::Integer(3)),
                "GDS-default validationFolds missing"
            );
        }
        other => panic!("splitConfig not a map: {other:?}"),
    }
    // autoTuningConfig GDS default maxTrials=10
    match &row[4] {
        CypherValue::Map(m) => assert_eq!(
            m.get("maxTrials"),
            Some(&CypherValue::Integer(10)),
            "GDS-default maxTrials missing"
        ),
        other => panic!("autoTuningConfig not a map: {other:?}"),
    }
    // parameterSpace starts empty
    match &row[5] {
        CypherValue::List(v) => assert!(v.is_empty()),
        other => panic!("parameterSpace not a list: {other:?}"),
    }
}

#[test]
fn list_pipelines_round_trip() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();

    // empty at first
    let empty = execute(
        &mut g,
        "CALL traverse.ml.pipeline.list() YIELD name RETURN name",
    )
    .unwrap();
    assert_eq!(empty.rows.len(), 0);

    // two pipelines
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('pa') \
         YIELD name RETURN name",
    )
    .unwrap();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('pb') \
         YIELD name RETURN name",
    )
    .unwrap();

    let listed = execute(
        &mut g,
        "CALL traverse.ml.pipeline.list() \
         YIELD name, taskType, candidateCount, featureStageCount \
         RETURN name, taskType, candidateCount, featureStageCount ORDER BY name",
    )
    .unwrap();
    assert_eq!(listed.rows.len(), 2);
    assert_eq!(listed.rows[0][0], CypherValue::String("pa".into()));
    assert_eq!(
        listed.rows[0][1],
        CypherValue::String("NodeClassification".into())
    );
    assert_eq!(listed.rows[0][2], CypherValue::Integer(0)); // candidateCount
    assert_eq!(listed.rows[0][3], CypherValue::Integer(0)); // featureStageCount
    assert_eq!(listed.rows[1][0], CypherValue::String("pb".into()));
}

#[test]
fn duplicate_create_is_a_clear_error() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') \
         YIELD name RETURN name",
    )
    .unwrap();
    let err = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') \
         YIELD name RETURN name",
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("already exists"),
        "expected 'already exists' in error, got: {msg}"
    );
}

#[test]
fn drop_pipeline_removes_it() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('todrop') \
         YIELD name RETURN name",
    )
    .unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.ml.pipeline.drop('todrop') \
         YIELD name, dropped RETURN name, dropped",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], CypherValue::String("todrop".into()));
    assert_eq!(r.rows[0][1], CypherValue::Bool(true));

    // list empty again
    let listed = execute(
        &mut g,
        "CALL traverse.ml.pipeline.list() YIELD name RETURN name",
    )
    .unwrap();
    assert_eq!(listed.rows.len(), 0);
}

#[test]
fn drop_nonexistent_pipeline_errors_clearly() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    let err = execute(
        &mut g,
        "CALL traverse.ml.pipeline.drop('never-existed') \
         YIELD name RETURN name",
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not found"),
        "expected 'not found' in error, got: {msg}"
    );
}

#[test]
fn add_node_property_appends_to_steps() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.addNodeProperty('p', 'degree', {}) \
         YIELD nodePropertySteps RETURN nodePropertySteps",
    )
    .unwrap();
    let steps = match &r.rows[0][0] {
        CypherValue::List(v) => v.clone(),
        other => panic!("nodePropertySteps not a list: {other:?}"),
    };
    assert_eq!(steps.len(), 1, "one step appended");
    match &steps[0] {
        CypherValue::Map(m) => {
            assert_eq!(
                m.get("procedureName"),
                Some(&CypherValue::String("degree".into())),
            );
        }
        other => panic!("step not a map: {other:?}"),
    }
}

#[test]
fn select_features_accepts_string_or_list() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();

    // single string
    let r1 = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.selectFeatures('p', 'pageRank') \
         YIELD featureProperties RETURN featureProperties",
    )
    .unwrap();
    match &r1.rows[0][0] {
        CypherValue::List(v) => {
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], CypherValue::String("pageRank".into()));
        }
        other => panic!("featureProperties not a list: {other:?}"),
    }

    // a list replaces wholesale (GDS semantics)
    let r2 = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.selectFeatures('p', ['degree','age']) \
         YIELD featureProperties RETURN featureProperties",
    )
    .unwrap();
    match &r2.rows[0][0] {
        CypherValue::List(v) => {
            assert_eq!(v.len(), 2);
            assert_eq!(v[0], CypherValue::String("degree".into()));
            assert_eq!(v[1], CypherValue::String("age".into()));
        }
        other => panic!("featureProperties not a list: {other:?}"),
    }
}

#[test]
fn configure_split_merges_into_defaults() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.configureSplit('p', {testFraction: 0.2}) \
         YIELD splitConfig RETURN splitConfig",
    )
    .unwrap();
    match &r.rows[0][0] {
        CypherValue::Map(m) => {
            // overridden
            assert_eq!(m.get("testFraction"), Some(&CypherValue::Float(0.2)));
            // default kept (merge, not replace)
            assert_eq!(m.get("validationFolds"), Some(&CypherValue::Integer(3)));
        }
        other => panic!("splitConfig not a map: {other:?}"),
    }
}

#[test]
fn configure_auto_tuning_overrides_max_trials() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.configureAutoTuning('p', {maxTrials: 25}) \
         YIELD autoTuningConfig RETURN autoTuningConfig",
    )
    .unwrap();
    match &r.rows[0][0] {
        CypherValue::Map(m) => {
            assert_eq!(m.get("maxTrials"), Some(&CypherValue::Integer(25)));
        }
        other => panic!("autoTuningConfig not a map: {other:?}"),
    }
}

#[test]
fn add_gcn_and_graph_sage_populate_parameter_space() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.addGcn('p', {hidden: 32}) \
         YIELD name RETURN name",
    )
    .unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.addGraphSage('p', {hidden: 16}) \
         YIELD parameterSpace RETURN parameterSpace",
    )
    .unwrap();
    let space = match &r.rows[0][0] {
        CypherValue::List(v) => v.clone(),
        other => panic!("parameterSpace not a list: {other:?}"),
    };
    assert_eq!(space.len(), 2, "two candidates added");
    let method = |i: usize| match &space[i] {
        CypherValue::Map(m) => match m.get("methodName") {
            Some(CypherValue::String(s)) => s.clone(),
            other => panic!("methodName not a string: {other:?}"),
        },
        other => panic!("candidate not a map: {other:?}"),
    };
    assert_eq!(method(0), "Gcn");
    assert_eq!(method(1), "GraphSage");
}

#[test]
fn config_procedures_on_unknown_pipeline_error_clearly() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    let err = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.addGcn('ghost', {hidden: 32}) \
         YIELD name RETURN name",
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("not found"), "expected 'not found' in: {msg}");
}

#[test]
fn keyword_segments_parse_in_procedure_names() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    // `explain` is a keyword; it still has to work as a namespace segment
    let err = execute(
        &mut g,
        "CALL traverse.ml.explain.counterfactual('g', {modelName: 'none', nodeId: 0}) \
         YIELD nodeId RETURN nodeId",
    )
    .unwrap_err()
    .to_string();
    assert!(!err.contains("syntax error"), "{err}");
}

#[test]
fn unknown_ml_procedure_is_a_clear_error() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    let err = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.bogus('x') \
         YIELD name RETURN name",
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not registered") || msg.contains("ProcedureNotFound"),
        "expected procedure-not-found error, got: {msg}"
    );
}

/// Train -> predict -> explain through the full executor; needs the `ml` feature (Burn).
#[cfg(feature = "ml")]
mod engine {
    use std::collections::BTreeMap;

    use super::*;
    use crate::executor::CypherResult;

    fn run(g: &mut Graph, q: &str) -> CypherResult {
        execute(g, q).unwrap_or_else(|e| panic!("{q}\n-> {e}"))
    }

    fn col<'a>(r: &'a CypherResult, row: usize, name: &str) -> &'a CypherValue {
        let i = r
            .columns
            .iter()
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", r.columns));
        &r.rows[row][i]
    }

    fn int(v: &CypherValue) -> i64 {
        match v {
            CypherValue::Integer(i) => *i,
            other => panic!("expected integer, got {other:?}"),
        }
    }

    fn float(v: &CypherValue) -> f64 {
        match v {
            CypherValue::Float(f) => *f,
            CypherValue::Integer(i) => *i as f64,
            other => panic!("expected number, got {other:?}"),
        }
    }

    fn map(v: &CypherValue) -> &BTreeMap<String, CypherValue> {
        match v {
            CypherValue::Map(m) => m,
            other => panic!("expected map, got {other:?}"),
        }
    }

    /// Two 15-node communities (dense KNOWS inside, LIKES on a subset, one bridge). `noise` is the
    /// community plus noise twice its size, so a single node is close to a coin flip and a model
    /// has to average over neighbors; `c` is the community, `score` a community-level number,
    /// `emb` a 3-element list.
    fn communities() -> Graph {
        let mut g = Graph::new();
        run(
            &mut g,
            "UNWIND range(0, 29) AS i CREATE (:P {i: i, \
             noise: CASE WHEN i < 15 THEN 0.0 ELSE 1.0 END + 0.5 * toFloat((i * 7) % 5), \
             c: CASE WHEN i < 15 THEN 0 ELSE 1 END, \
             score: CASE WHEN i < 15 THEN 10.0 ELSE 30.0 END, \
             emb: [toFloat(i % 3), 1.0, toFloat(i % 2)]})",
        );
        run(
            &mut g,
            "MATCH (a:P), (b:P) WHERE a.i < b.i AND (a.i < 15) = (b.i < 15) \
             AND (a.i * 7 + b.i * 3) % 4 <> 0 CREATE (a)-[:KNOWS]->(b)",
        );
        run(
            &mut g,
            "MATCH (a:P), (b:P) WHERE a.i < b.i AND (a.i < 15) = (b.i < 15) \
             AND (a.i + b.i) % 5 = 0 CREATE (a)-[:LIKES]->(b)",
        );
        run(
            &mut g,
            "MATCH (a:P {i: 0}), (b:P {i: 15}) CREATE (a)-[:KNOWS]->(b)",
        );
        g
    }

    fn pipeline(g: &mut Graph, task: &str, name: &str, features: &str, candidates: &[&str]) {
        run(
            g,
            &format!("CALL traverse.ml.pipeline.{task}.create('{name}') YIELD name RETURN name"),
        );
        run(
            g,
            &format!(
                "CALL traverse.ml.pipeline.{task}.selectFeatures('{name}', {features}) \
                 YIELD name RETURN name"
            ),
        );
        for c in candidates {
            run(
                g,
                &format!("CALL traverse.ml.pipeline.{task}.{c} YIELD name RETURN name"),
            );
        }
    }

    fn train(g: &mut Graph, task: &str, config: &str) -> CypherResult {
        let r = run(
            g,
            &format!(
                "CALL traverse.ml.pipeline.{task}.train('g', {{wait: true, epochs: 60, {config}}}) \
                 YIELD jobId, status, trainMillis, modelInfo, modelSelectionStats, error \
                 RETURN jobId, status, trainMillis, modelInfo, modelSelectionStats, error"
            ),
        );
        assert_eq!(
            col(&r, 0, "status"),
            &CypherValue::String("completed".into()),
            "training failed: {:?}",
            col(&r, 0, "error")
        );
        r
    }

    #[test]
    fn node_classification_selects_trains_predicts_and_writes() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "nc",
            "['noise']",
            &[
                "addGcn('nc', {hidden: 16, learningRate: 0.02})",
                "addGraphSage('nc', {hidden: {range: [8, 24]}, learningRate: {range: [0.005, 0.05]}})",
            ],
        );
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.configureAutoTuning('nc', {maxTrials: 3}) \
             YIELD name RETURN name",
        );
        let r = train(
            &mut g,
            "nodeClassification",
            "pipeline: 'nc', targetProperty: 'c', modelName: 'nc-model', metrics: ['ACCURACY', 'F1_MACRO']",
        );
        let info = map(col(&r, 0, "modelInfo"));
        let metrics = map(&info["metrics"]);
        let acc = map(&metrics["ACCURACY"]);
        assert!(
            float(&acc["test"]) >= 0.75,
            "test accuracy {:?}",
            acc["test"]
        );
        assert!(map(&acc["validation"]).contains_key("avg"));
        let stats = map(col(&r, 0, "modelSelectionStats"));
        match &stats["modelCandidates"] {
            CypherValue::List(c) => assert_eq!(c.len(), 3, "1 concrete + 2 sampled trials"),
            other => panic!("modelCandidates: {other:?}"),
        }

        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.predict.stream('g', \
             {modelName: 'nc-model', includePredictedProbabilities: true}) \
             YIELD nodeId, predictedClass, predictedProbabilities \
             RETURN nodeId, predictedClass, predictedProbabilities",
        );
        assert_eq!(p.rows.len(), 30);
        let probs = match col(&p, 0, "predictedProbabilities") {
            CypherValue::List(v) => v.iter().map(float).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(probs.len(), 2);
        assert!((probs.iter().sum::<f64>() - 1.0).abs() < 1e-4);

        // write into a fresh property, then compare with the truth
        let w = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.predict.write('g', \
             {modelName: 'nc-model', writeProperty: 'predicted'}) \
             YIELD nodesWritten RETURN nodesWritten",
        );
        assert_eq!(int(col(&w, 0, "nodesWritten")), 30);
        let ok = run(
            &mut g,
            "MATCH (n:P) WHERE n.predicted = n.c RETURN count(n) AS ok",
        );
        assert!(int(col(&ok, 0, "ok")) >= 25, "{:?} correct", ok.rows);
    }

    #[test]
    fn node_regression_with_list_features_and_successive_halving() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeRegression",
            "nr",
            "['noise', 'emb']",
            &["addGcn('nr', {hidden: {range: [8, 32]}, learningRate: {range: [0.005, 0.05]}})"],
        );
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeRegression.configureAutoTuning('nr', \
             {maxTrials: 4, strategy: 'successiveHalving', eta: 2, minEpochs: 15}) \
             YIELD name RETURN name",
        );
        let r = train(
            &mut g,
            "nodeRegression",
            "pipeline: 'nr', targetProperty: 'score', modelName: 'nr-model'",
        );
        let info = map(col(&r, 0, "modelInfo"));
        let rmse = float(&map(&map(&info["metrics"])["ROOT_MEAN_SQUARED_ERROR"])["test"]);
        assert!(rmse < 6.0, "test RMSE {rmse}");
        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeRegression.predict.stream('g', {modelName: 'nr-model'}) \
             YIELD nodeId, predictedValue RETURN nodeId, predictedValue",
        );
        assert_eq!(p.rows.len(), 30);
        let mean: f64 = p.rows.iter().map(|r| float(&r[1])).sum::<f64>() / 30.0;
        assert!((mean - 20.0).abs() < 5.0, "mean prediction {mean}");
    }

    #[test]
    fn rgcn_and_gat_train_on_typed_relationships() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "typed",
            "'noise'",
            &[
                "addRGcn('typed', {hidden: 16, learningRate: 0.02})",
                "addGat('typed', {hidden: 8, heads: 2, learningRate: 0.02})",
            ],
        );
        let r = train(
            &mut g,
            "nodeClassification",
            "pipeline: 'typed', targetProperty: 'c', modelName: 'typed-model'",
        );
        let best = map(&map(col(&r, 0, "modelInfo"))["bestParameters"]);
        assert!(
            matches!(&best["methodName"], CypherValue::String(s) if s == "RGcn" || s == "Gat"),
            "{best:?}"
        );
        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.predict.stream('g', {modelName: 'typed-model'}) \
             YIELD predictedClass RETURN count(*) AS n",
        );
        assert_eq!(int(col(&p, 0, "n")), 30);
    }

    #[test]
    fn link_prediction_ranks_and_writes_new_relationships() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "linkPrediction",
            "lp",
            "['noise', 'emb']",
            &["addGcn('lp', {hidden: 16, learningRate: 0.02})"],
        );
        let r = train(
            &mut g,
            "linkPrediction",
            "pipeline: 'lp', targetRelationshipType: 'KNOWS', modelName: 'lp-model', embeddingDimension: 16",
        );
        let info = map(col(&r, 0, "modelInfo"));
        let auc = float(&map(&map(&info["metrics"])["ROC_AUC"])["test"]);
        assert!(auc > 0.6, "test ROC-AUC {auc}");
        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.linkPrediction.predict.stream('g', {modelName: 'lp-model', topN: 5}) \
             YIELD node1, node2, probability RETURN node1, node2, probability",
        );
        assert_eq!(p.rows.len(), 5);
        let probs: Vec<f64> = p.rows.iter().map(|r| float(&r[2])).collect();
        assert!(
            probs.windows(2).all(|w| w[0] >= w[1]),
            "not sorted: {probs:?}"
        );
        let w = run(
            &mut g,
            "CALL traverse.ml.pipeline.linkPrediction.predict.write('g', \
             {modelName: 'lp-model', topN: 3, writeRelationshipType: 'SUGGESTED'}) \
             YIELD relationshipsWritten RETURN relationshipsWritten",
        );
        assert_eq!(int(col(&w, 0, "relationshipsWritten")), 3);
        let n = run(
            &mut g,
            "MATCH ()-[r:SUGGESTED]->() RETURN count(r) AS n, min(r.probability) AS p",
        );
        assert_eq!(int(col(&n, 0, "n")), 3);
        assert!(float(col(&n, 0, "p")) > 0.0);
    }

    #[test]
    fn node_property_steps_feed_features() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.create('steps') YIELD name RETURN name",
        );
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.addNodeProperty('steps', 'gds.pageRank.mutate', \
             {mutateProperty: 'pr'}) YIELD name RETURN name",
        );
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.addNodeProperty('steps', 'graphMae', \
             {featureProperties: ['emb', 'noise'], embeddingDimension: 4, epochs: 20, mutateProperty: 'mae'}) \
             YIELD name RETURN name",
        );
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.selectFeatures('steps', ['pr', 'mae']) \
             YIELD name RETURN name",
        );
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.addGcn('steps', {hidden: 8}) YIELD name RETURN name",
        );
        let r = train(
            &mut g,
            "nodeClassification",
            "pipeline: 'steps', targetProperty: 'c', modelName: 'steps-model'",
        );
        let info = map(col(&r, 0, "modelInfo"));
        assert!(float(&map(&map(&info["metrics"])["ACCURACY"])["test"]) >= 0.5);
        // the steps re-run at prediction time; nothing was written to the database
        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.predict.stream('g', {modelName: 'steps-model'}) \
             YIELD predictedClass RETURN count(*) AS n",
        );
        assert_eq!(int(col(&p, 0, "n")), 30);
        let none = run(
            &mut g,
            "MATCH (n:P) WHERE n.pr IS NOT NULL RETURN count(n) AS n",
        );
        assert_eq!(int(col(&none, 0, "n")), 0);
    }

    #[test]
    fn graph_mae_streams_and_writes_vectors() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        let s = run(
            &mut g,
            "CALL traverse.ml.graphMae.stream('g', {featureProperties: ['emb', 'noise'], \
             embeddingDimension: 6, epochs: 20}) YIELD nodeId, embedding RETURN nodeId, embedding",
        );
        assert_eq!(s.rows.len(), 30);
        match col(&s, 0, "embedding") {
            CypherValue::List(v) => assert_eq!(v.len(), 6),
            other => panic!("{other:?}"),
        }
        let w = run(
            &mut g,
            "CALL traverse.ml.graphMae.write('g', {featureProperties: 'emb', embeddingDimension: 4, \
             epochs: 10, writeProperty: 'mae'}) YIELD nodePropertiesWritten RETURN nodePropertiesWritten",
        );
        assert_eq!(int(col(&w, 0, "nodePropertiesWritten")), 30);
        let v = run(&mut g, "MATCH (n:P {i: 3}) RETURN size(n.mae) AS d");
        assert_eq!(int(col(&v, 0, "d")), 4);
    }

    #[test]
    fn explainers_name_relationships_of_the_node() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "ex",
            "'noise'",
            &["addGcn('ex', {hidden: 16, learningRate: 0.02})"],
        );
        train(
            &mut g,
            "nodeClassification",
            "pipeline: 'ex', targetProperty: 'c', modelName: 'ex-model'",
        );
        let node = run(&mut g, "MATCH (n:P {i: 4}) RETURN id(n) AS id");
        let id = int(col(&node, 0, "id"));

        let cf = run(
            &mut g,
            &format!(
                "CALL traverse.ml.explain.counterfactual('g', {{modelName: 'ex-model', nodeId: {id}, epochs: 60}}) \
                 YIELD nodeId, originalClass, counterfactualClass, flipped, removedRelationships \
                 RETURN nodeId, originalClass, counterfactualClass, flipped, removedRelationships"
            ),
        );
        assert_eq!(int(col(&cf, 0, "nodeId")), id);
        assert!(matches!(
            col(&cf, 0, "removedRelationships"),
            CypherValue::List(_)
        ));

        let audit = run(
            &mut g,
            &format!(
                "CALL traverse.ml.explain.subgraphAudit('g', {{modelName: 'ex-model', nodeId: {id}, maxRelationships: 3}}) \
                 YIELD predictedClass, baselineProbability, relationships \
                 RETURN predictedClass, baselineProbability, relationships"
            ),
        );
        match col(&audit, 0, "relationships") {
            CypherValue::List(v) => assert!(!v.is_empty() && v.len() <= 3, "{v:?}"),
            other => panic!("{other:?}"),
        }

        let pg = run(
            &mut g,
            &format!(
                "CALL traverse.ml.explain.pgExplainer('g', {{modelName: 'ex-model', nodeIds: [{id}], topK: 4, epochs: 10}}) \
                 YIELD nodeId, sourceNodeId, targetNodeId, importance \
                 RETURN nodeId, sourceNodeId, targetNodeId, importance"
            ),
        );
        assert!(!pg.rows.is_empty() && pg.rows.len() <= 4);
        let imp: Vec<f64> = pg.rows.iter().map(|r| float(&r[3])).collect();
        assert!(imp.windows(2).all(|w| w[0] >= w[1]), "{imp:?}");
    }

    #[test]
    fn async_training_reports_progress_then_completes() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "bg",
            "'noise'",
            &["addGcn('bg', {hidden: 8})"],
        );
        let start = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.train('g', \
             {pipeline: 'bg', targetProperty: 'c', modelName: 'bg-model', epochs: 30}) \
             YIELD jobId, status RETURN jobId, status",
        );
        let job = match col(&start, 0, "jobId") {
            CypherValue::String(s) => s.clone(),
            other => panic!("{other:?}"),
        };
        let mut status = String::new();
        for _ in 0..600 {
            let s = run(
                &mut g,
                &format!(
                    "CALL traverse.ml.trainStatus('{job}') YIELD status, progress, error \
                     RETURN status, progress, error"
                ),
            );
            status = match col(&s, 0, "status") {
                CypherValue::String(s) => s.clone(),
                other => panic!("{other:?}"),
            };
            if status != "queued" && status != "running" {
                assert_eq!(status, "completed", "{:?}", col(&s, 0, "error"));
                let progress = map(col(&s, 0, "progress"));
                assert_eq!(int(&progress["completedTrials"]), 1);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(status, "completed");
    }

    fn text(v: &CypherValue) -> &str {
        match v {
            CypherValue::String(s) => s,
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn mini_batches_on_a_chosen_device_train_and_predict() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "mb",
            "'noise'",
            &["addGraphSage('mb', {hidden: 16, learningRate: 0.02})"],
        );
        let r = train(
            &mut g,
            "nodeClassification",
            "pipeline: 'mb', targetProperty: 'c', modelName: 'mb-model', device: 'cpu', \
             batchSize: 8, fanouts: [4, -1]",
        );
        let info = map(col(&r, 0, "modelInfo"));
        assert_eq!(text(&info["device"]), "cpu");
        let batching = map(&info["batching"]);
        assert_eq!(text(&batching["mode"]), "miniBatch");
        assert_eq!(int(&batching["batchSize"]), 8);
        assert_eq!(
            batching["fanouts"],
            CypherValue::List(vec![CypherValue::Integer(4), CypherValue::Integer(-1)])
        );
        let test_acc = float(&map(&map(&info["metrics"])["ACCURACY"])["test"]);
        assert!(test_acc >= 0.7, "{test_acc}");
        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.predict.stream('g', \
             {modelName: 'mb-model', device: 'cpu'}) YIELD nodeId, predictedClass \
             RETURN count(*) AS n",
        );
        assert_eq!(int(col(&p, 0, "n")), 30);
    }

    #[test]
    fn jobs_are_listed_and_can_be_cancelled() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "long",
            "'noise'",
            &["addGcn('long', {hidden: 8})"],
        );
        // no early stopping and a huge budget: it runs until cancelled
        let start = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.train('g', {pipeline: 'long', \
             targetProperty: 'c', modelName: 'long-model', epochs: 100000000, patience: 0}) \
             YIELD jobId RETURN jobId",
        );
        let job = text(col(&start, 0, "jobId")).to_string();
        let listed = run(
            &mut g,
            "CALL traverse.ml.trainJobs() YIELD jobId, status, modelName, device \
             RETURN jobId, status, modelName, device",
        );
        let mine = (0..listed.rows.len())
            .find(|&i| text(col(&listed, i, "jobId")) == job)
            .expect("the job is listed");
        assert_eq!(text(col(&listed, mine, "modelName")), "long-model");
        assert!(matches!(
            text(col(&listed, mine, "status")),
            "queued" | "running"
        ));
        let c = run(
            &mut g,
            &format!(
                "CALL traverse.ml.trainCancel('{job}') YIELD cancelRequested \
                 RETURN cancelRequested"
            ),
        );
        assert_eq!(col(&c, 0, "cancelRequested"), &CypherValue::Bool(true));
        let mut status = String::new();
        for _ in 0..600 {
            let s = run(
                &mut g,
                &format!("CALL traverse.ml.trainStatus('{job}') YIELD status RETURN status"),
            );
            status = text(col(&s, 0, "status")).to_string();
            if status == "cancelled" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(status, "cancelled");
        // no model came of it
        let models = run(
            &mut g,
            "CALL traverse.ml.model.list() YIELD modelName WHERE modelName = 'long-model' \
             RETURN count(*) AS n",
        );
        assert_eq!(int(col(&models, 0, "n")), 0);
    }

    /// GPU validation runs (`TRAVERSE_EXPECT_GPU`, a `ml-cuda` build) train, predict, embed and
    /// explain on the GPU by name, so a broken GPU path can't hide behind `auto` falling back.
    #[test]
    fn everything_runs_on_the_gpu_when_one_is_expected() {
        if std::env::var_os("TRAVERSE_EXPECT_GPU").is_none() {
            return;
        }
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "gpu",
            "'noise'",
            &[
                "addGcn('gpu', {hidden: 16, learningRate: 0.02})",
                "addGat('gpu', {hidden: 8, heads: 2, learningRate: 0.02})",
            ],
        );
        for (name, batching) in [
            ("gpu-full", "batching: 'fullGraph'"),
            ("gpu-mb", "batchSize: 8"),
        ] {
            let r = train(
                &mut g,
                "nodeClassification",
                &format!(
                    "pipeline: 'gpu', targetProperty: 'c', modelName: '{name}', device: 'gpu', \
                     {batching}"
                ),
            );
            let info = map(col(&r, 0, "modelInfo"));
            assert!(text(&info["device"]).starts_with("cuda:"), "{info:?}");
            let test_acc = float(&map(&map(&info["metrics"])["ACCURACY"])["test"]);
            assert!(test_acc >= 0.7, "{name}: {test_acc}");
            let p = run(
                &mut g,
                &format!(
                    "CALL traverse.ml.pipeline.nodeClassification.predict.stream('g', \
                     {{modelName: '{name}', device: 'gpu'}}) YIELD nodeId, predictedClass \
                     MATCH (n) WHERE id(n) = nodeId \
                     RETURN sum(CASE WHEN n.c = predictedClass THEN 1 ELSE 0 END) AS right"
                ),
            );
            assert!(int(col(&p, 0, "right")) >= 21, "{name}: {p:?}");
        }
        let mae = run(
            &mut g,
            "CALL traverse.ml.graphMae.stream('g', {featureProperties: ['noise', 'emb'], \
             embeddingDimension: 8, epochs: 20, device: 'gpu'}) YIELD nodeId, embedding \
             RETURN count(*) AS n",
        );
        assert_eq!(int(col(&mae, 0, "n")), 30);
        let node = run(&mut g, "MATCH (n:P {i: 4}) RETURN id(n) AS id");
        let id = int(col(&node, 0, "id"));
        let cf = run(
            &mut g,
            &format!(
                "CALL traverse.ml.explain.counterfactual('g', {{modelName: 'gpu-full', \
                 nodeId: {id}, epochs: 30, device: 'gpu'}}) YIELD nodeId RETURN nodeId"
            ),
        );
        assert_eq!(int(col(&cf, 0, "nodeId")), id);
    }

    #[test]
    fn naming_classes_keeps_predicting_the_class_values() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        run(
            &mut g,
            "MATCH (n:P) SET n.k = CASE n.c WHEN 0 THEN 3 ELSE 7 END",
        );
        pipeline(
            &mut g,
            "nodeClassification",
            "named",
            "'noise'",
            &["addGcn('named', {hidden: 16, learningRate: 0.02})"],
        );
        train(
            &mut g,
            "nodeClassification",
            "pipeline: 'named', targetProperty: 'k', modelName: 'named-model'",
        );
        // what Studio does when someone names the classes
        run(
            &mut g,
            "CALL traverse.ml.model.setMetadata('named-model', 'classLabels', \
             '{\"0\":\"low\",\"1\":\"high\"}') YIELD modelName RETURN modelName",
        );
        let p = run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.predict.stream('g', \
             {modelName: 'named-model'}) YIELD predictedClass \
             RETURN collect(DISTINCT predictedClass) AS classes",
        );
        match col(&p, 0, "classes") {
            CypherValue::List(v) => {
                assert!(!v.is_empty());
                for c in v {
                    assert!(matches!(c, CypherValue::Integer(3 | 7)), "{v:?}");
                }
            }
            other => panic!("{other:?}"),
        }
        let listed = run(
            &mut g,
            "CALL traverse.ml.model.list() YIELD modelName, taskType, classLabels, testMetrics \
             WHERE modelName = 'named-model' RETURN taskType, classLabels, testMetrics",
        );
        assert_eq!(text(col(&listed, 0, "taskType")), "NodeClassification");
        assert!(text(col(&listed, 0, "classLabels")).contains("high"));
        assert!(map(col(&listed, 0, "testMetrics")).contains_key("ACCURACY"));
    }

    #[test]
    fn devices_lists_the_cpu_first() {
        let mut g = Graph::new();
        let r = run(
            &mut g,
            "CALL traverse.ml.devices() YIELD device, type, available, reason \
             RETURN device, type, available, reason",
        );
        assert_eq!(text(col(&r, 0, "device")), "cpu");
        assert_eq!(col(&r, 0, "available"), &CypherValue::Bool(true));
        assert!(r.rows.len() >= 2, "a GPU row says what's there or why not");
    }

    #[test]
    fn bad_device_and_batching_settings_are_clear_errors() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        pipeline(
            &mut g,
            "nodeClassification",
            "bad",
            "'noise'",
            &["addGcn('bad', {})"],
        );
        for (setting, expect) in [
            ("device: 'tpu'", "unknown device"),
            ("device: 'gpu:99'", "99"),
            ("batching: 'fullGraph', batchSize: 8", "miniBatch"),
            ("fanouts: [10]", "fanouts"),
        ] {
            let err = execute(
                &mut g,
                &format!(
                    "CALL traverse.ml.pipeline.nodeClassification.train('g', {{pipeline: 'bad', \
                     targetProperty: 'c', modelName: 'm', wait: true, {setting}}}) \
                     YIELD status RETURN status"
                ),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains(expect), "{setting}: {err}");
        }
    }

    #[test]
    fn mismatched_task_and_bad_config_are_clear_errors() {
        let _catalog = fresh_catalog();
        let mut g = communities();
        run(
            &mut g,
            "CALL traverse.ml.pipeline.nodeRegression.create('r') YIELD name RETURN name",
        );
        let wrong = execute(
            &mut g,
            "CALL traverse.ml.pipeline.nodeClassification.addGcn('r', {}) YIELD name RETURN name",
        )
        .unwrap_err()
        .to_string();
        assert!(wrong.contains("NodeRegression pipeline"), "{wrong}");
        let folds = execute(
            &mut g,
            "CALL traverse.ml.pipeline.nodeRegression.configureSplit('r', {validationFolds: 1}) \
             YIELD name RETURN name",
        )
        .unwrap_err()
        .to_string();
        assert!(folds.contains("validationFolds"), "{folds}");
        let heads = execute(
            &mut g,
            "CALL traverse.ml.pipeline.nodeRegression.addGcn('r', {heads: 4}) YIELD name RETURN name",
        )
        .unwrap_err()
        .to_string();
        assert!(heads.contains("heads"), "{heads}");
        let metric = {
            run(
                &mut g,
                "CALL traverse.ml.pipeline.nodeRegression.selectFeatures('r', 'noise') YIELD name RETURN name",
            );
            run(
                &mut g,
                "CALL traverse.ml.pipeline.nodeRegression.addGcn('r', {}) YIELD name RETURN name",
            );
            execute(
                &mut g,
                "CALL traverse.ml.pipeline.nodeRegression.train('g', {pipeline: 'r', modelName: 'x', \
                 targetProperty: 'score', metrics: ['ACCURACY'], wait: true}) YIELD status RETURN status",
            )
            .unwrap_err()
            .to_string()
        };
        assert!(metric.contains("MEAN_SQUARED_ERROR"), "{metric}");
    }
}

/// Persists to a temporary data directory for one test; stops persisting when dropped, even if
/// the test fails, so later tests don't write into a deleted directory.
struct DataDir(tempfile::TempDir);

impl DataDir {
    fn open() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(super::registry::open_data_dir(dir.path()), Ok(0));
        Self(dir)
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        super::registry::close_data_dir();
    }
}

#[test]
fn pipelines_survive_a_restart() {
    let _catalog = fresh_catalog();
    let dir = DataDir::open();
    let mut g = empty_graph();
    for q in [
        "CALL traverse.ml.pipeline.nodeRegression.create('kept') YIELD name RETURN name",
        "CALL traverse.ml.pipeline.nodeRegression.selectFeatures('kept', ['age', 'pr']) \
         YIELD name RETURN name",
        "CALL traverse.ml.pipeline.nodeRegression.addGat('kept', \
         {hidden: {range: [8, 32]}, heads: 2}) YIELD name RETURN name",
        "CALL traverse.ml.pipeline.linkPrediction.create('gone') YIELD name RETURN name",
        "CALL traverse.ml.pipeline.drop('gone') YIELD name RETURN name",
    ] {
        execute(&mut g, q).unwrap();
    }
    // a restart: memory gone, the catalog file read back
    super::registry::clear();
    assert_eq!(super::registry::open_data_dir(dir.0.path()), Ok(1));
    let p = super::registry::snapshot("kept").expect("reloaded");
    assert_eq!(p.task_type, super::registry::TaskType::NodeRegression);
    assert_eq!(p.feature_properties, vec!["age", "pr"]);
    assert_eq!(p.parameter_space.len(), 1);
    assert!(super::registry::snapshot("gone").is_none());
}

#[test]
fn settings_that_cant_be_stored_are_refused() {
    let _catalog = fresh_catalog();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();
    let err = execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.addNodeProperty('p', 'pageRank', \
         {asOf: date('2020-01-01')}) YIELD name RETURN name",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("plain values"), "{err}");
    assert!(
        super::registry::snapshot("p")
            .unwrap()
            .node_property_steps
            .is_empty()
    );
}

#[test]
fn a_change_that_cant_be_saved_changes_nothing() {
    let _catalog = fresh_catalog();
    let dir = DataDir::open();
    let mut g = empty_graph();
    execute(
        &mut g,
        "CALL traverse.ml.pipeline.nodeClassification.create('p') YIELD name RETURN name",
    )
    .unwrap();
    // the directory disappears under the server
    let path = dir.0.path().to_path_buf();
    std::fs::remove_dir_all(&path).unwrap();
    for q in [
        "CALL traverse.ml.pipeline.nodeClassification.addGcn('p', {}) YIELD name RETURN name",
        "CALL traverse.ml.pipeline.nodeClassification.create('q') YIELD name RETURN name",
        "CALL traverse.ml.pipeline.drop('p') YIELD name RETURN name",
    ] {
        let err = execute(&mut g, q).unwrap_err().to_string();
        assert!(err.contains("couldn't save"), "{q}: {err}");
    }
    let p = super::registry::snapshot("p").expect("still there");
    assert!(p.parameter_space.is_empty());
    assert!(super::registry::snapshot("q").is_none());
    std::fs::create_dir_all(&path).unwrap();
}
