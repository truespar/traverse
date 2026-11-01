//! Evaluation metrics for classification, regression and link prediction.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::pipeline::TaskType;

/// Evaluation metric, spelled as in GDS. The first metric of a training run selects the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Metric {
    /// Share of correct class predictions.
    Accuracy,
    /// Per-class F1 averaged with support weights.
    F1Weighted,
    /// Per-class F1, unweighted mean over the classes present.
    F1Macro,
    /// Mean squared error.
    MeanSquaredError,
    /// Square root of the MSE, in target units.
    RootMeanSquaredError,
    /// Mean absolute error.
    MeanAbsoluteError,
    /// Area under the precision-recall curve (average precision).
    Aucpr,
    /// Area under the ROC curve.
    RocAuc,
}

impl Metric {
    /// GDS spelling, e.g. `F1_WEIGHTED`.
    pub fn name(self) -> &'static str {
        match self {
            Metric::Accuracy => "ACCURACY",
            Metric::F1Weighted => "F1_WEIGHTED",
            Metric::F1Macro => "F1_MACRO",
            Metric::MeanSquaredError => "MEAN_SQUARED_ERROR",
            Metric::RootMeanSquaredError => "ROOT_MEAN_SQUARED_ERROR",
            Metric::MeanAbsoluteError => "MEAN_ABSOLUTE_ERROR",
            Metric::Aucpr => "AUCPR",
            Metric::RocAuc => "ROC_AUC",
        }
    }

    /// Case-insensitive parse of a GDS metric name.
    pub fn parse(s: &str) -> Option<Self> {
        let all = [
            Metric::Accuracy,
            Metric::F1Weighted,
            Metric::F1Macro,
            Metric::MeanSquaredError,
            Metric::RootMeanSquaredError,
            Metric::MeanAbsoluteError,
            Metric::Aucpr,
            Metric::RocAuc,
        ];
        all.into_iter().find(|m| m.name().eq_ignore_ascii_case(s))
    }

    /// Larger values are better (accuracy) or worse (errors).
    pub fn higher_is_better(self) -> bool {
        !matches!(
            self,
            Metric::MeanSquaredError | Metric::RootMeanSquaredError | Metric::MeanAbsoluteError
        )
    }

    /// The task this metric applies to.
    pub fn task(self) -> TaskType {
        match self {
            Metric::Accuracy | Metric::F1Weighted | Metric::F1Macro => TaskType::NodeClassification,
            Metric::MeanSquaredError | Metric::RootMeanSquaredError | Metric::MeanAbsoluteError => {
                TaskType::NodeRegression
            }
            Metric::Aucpr | Metric::RocAuc => TaskType::LinkPrediction,
        }
    }

    /// Default metric list per task, the first selecting the model.
    pub fn defaults(task: TaskType) -> Vec<Metric> {
        match task {
            TaskType::NodeClassification => {
                vec![Metric::Accuracy, Metric::F1Weighted, Metric::F1Macro]
            }
            TaskType::NodeRegression => vec![
                Metric::MeanSquaredError,
                Metric::RootMeanSquaredError,
                Metric::MeanAbsoluteError,
            ],
            TaskType::LinkPrediction => vec![Metric::Aucpr, Metric::RocAuc],
        }
    }

    /// `value` turned into a score where larger is always better.
    pub(super) fn score(self, value: f64) -> f64 {
        if self.higher_is_better() {
            value
        } else {
            -value
        }
    }
}

/// Precision / recall / F1 / support of one class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassReport {
    pub class: i32,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    pub support: usize,
}

/// Classification metrics plus per-class report over `idx`.
pub fn classification_metrics(
    labels: &[i32],
    preds: &[i32],
    idx: &[u32],
    num_classes: usize,
) -> (BTreeMap<Metric, f64>, Vec<ClassReport>) {
    let mut tp = vec![0usize; num_classes];
    let mut fp = vec![0usize; num_classes];
    let mut fneg = vec![0usize; num_classes];
    let mut support = vec![0usize; num_classes];
    let mut correct = 0usize;
    for &i in idx {
        let (a, p) = (labels[i as usize], preds[i as usize]);
        if a < 0 || a as usize >= num_classes {
            continue;
        }
        support[a as usize] += 1;
        if a == p {
            correct += 1;
            tp[a as usize] += 1;
        } else {
            fneg[a as usize] += 1;
            if p >= 0 && (p as usize) < num_classes {
                fp[p as usize] += 1;
            }
        }
    }
    let total: usize = support.iter().sum();
    let ratio = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f64 / b as f64 };
    let mut reports = Vec::with_capacity(num_classes);
    let (mut macro_sum, mut weighted_sum, mut present) = (0.0, 0.0, 0usize);
    for c in 0..num_classes {
        let precision = ratio(tp[c], tp[c] + fp[c]);
        let recall = ratio(tp[c], tp[c] + fneg[c]);
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };
        if support[c] > 0 {
            macro_sum += f1;
            weighted_sum += f1 * support[c] as f64;
            present += 1;
        }
        reports.push(ClassReport {
            class: c as i32,
            precision,
            recall,
            f1,
            support: support[c],
        });
    }
    let mut m = BTreeMap::new();
    m.insert(Metric::Accuracy, ratio(correct, total));
    m.insert(
        Metric::F1Macro,
        if present > 0 {
            macro_sum / present as f64
        } else {
            0.0
        },
    );
    m.insert(
        Metric::F1Weighted,
        if total > 0 {
            weighted_sum / total as f64
        } else {
            0.0
        },
    );
    (m, reports)
}

/// Regression errors over `idx`, in target units.
pub fn regression_metrics(targets: &[f32], preds: &[f32], idx: &[u32]) -> BTreeMap<Metric, f64> {
    let (mut se, mut ae, mut n) = (0.0f64, 0.0f64, 0usize);
    for &i in idx {
        let t = targets[i as usize];
        if t.is_nan() {
            continue;
        }
        let d = (preds[i as usize] - t) as f64;
        se += d * d;
        ae += d.abs();
        n += 1;
    }
    let mse = if n > 0 { se / n as f64 } else { 0.0 };
    let mut m = BTreeMap::new();
    m.insert(Metric::MeanSquaredError, mse);
    m.insert(Metric::RootMeanSquaredError, mse.sqrt());
    m.insert(
        Metric::MeanAbsoluteError,
        if n > 0 { ae / n as f64 } else { 0.0 },
    );
    m
}

/// AUCPR (average precision) and ROC-AUC (Mann-Whitney U, ties counting half) of positive vs
/// negative scores.
pub fn link_metrics(pos: &[f32], neg: &[f32]) -> BTreeMap<Metric, f64> {
    let mut m = BTreeMap::new();
    if pos.is_empty() || neg.is_empty() {
        m.insert(Metric::Aucpr, 0.0);
        m.insert(Metric::RocAuc, 0.0);
        return m;
    }
    let mut all: Vec<(f32, bool)> = pos
        .iter()
        .map(|&s| (s, true))
        .chain(neg.iter().map(|&s| (s, false)))
        .collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    // ROC-AUC from average ranks
    let mut rank_sum_pos = 0.0f64;
    let mut i = 0;
    while i < all.len() {
        let mut j = i;
        while j + 1 < all.len() && all[j + 1].0 == all[i].0 {
            j += 1;
        }
        let avg_rank = (i + j) as f64 / 2.0 + 1.0;
        for item in &all[i..=j] {
            if item.1 {
                rank_sum_pos += avg_rank;
            }
        }
        i = j + 1;
    }
    let (np, nn) = (pos.len() as f64, neg.len() as f64);
    let auc = (rank_sum_pos - np * (np + 1.0) / 2.0) / (np * nn);
    // average precision, scores descending, tied scores processed as one threshold
    let mut ap = 0.0f64;
    let (mut tp, mut seen) = (0usize, 0usize);
    let mut k = all.len();
    while k > 0 {
        let mut start = k - 1;
        while start > 0 && all[start - 1].0 == all[k - 1].0 {
            start -= 1;
        }
        let group_pos = all[start..k].iter().filter(|x| x.1).count();
        tp += group_pos;
        seen += k - start;
        if group_pos > 0 {
            ap += (group_pos as f64 / np) * (tp as f64 / seen as f64);
        }
        k = start;
    }
    m.insert(Metric::Aucpr, ap);
    m.insert(Metric::RocAuc, auc);
    m
}
