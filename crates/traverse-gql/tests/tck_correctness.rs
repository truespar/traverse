//! Third TCK harness: parse -> lower -> execute -> **compare the returned rows
//! with the `Then the result should be: ...` table**. The strictest conformance
//! level on this side of the dialect bridge.
//!
//! Layers, weakest -> strongest:
//! * `tck.rs`: does the grammar accept the query?
//! * `tck_execute.rs`: does it run on the real engine without error?
//! * `tck_correctness.rs`: does it produce exactly the rows the spec wants?
//!
//! Comparison rules:
//!
//! * Each cell is normalized to a canonical string: `CypherValue::Bool(true)`
//!   -> `"true"`, lists -> `"[a, b, c]"`, strings keep their single-quoted form
//!   `"'foo'"`, matching the TCK's literal style so string equality compares.
//! * "the result should be, in any order" sorts both sides by canonical row
//!   form first; otherwise comparison is positional.
//! * A cell with no canonical form (Path, refs, temporals) makes the scenario
//!   **incomparable**, excluded from the match/mismatch counts.

use std::path::{Path, PathBuf};

use gherkin::{Feature, Scenario, Step};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

mod common;
use common::{GqlCatalog, StmtOutcome, expand_scenarios, run_program};

fn tck_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/opengql_tck")
}

#[test]
fn opengql_tck_correctness() {
    let root = tck_root();
    let features_dir = root.join("features");
    if !features_dir.exists() {
        eprintln!(
            "opengql/tck submodule not initialised at {}; skipping.",
            features_dir.display()
        );
        return;
    }

    let mut stats = Stats::default();
    let feature_paths = collect_features(&features_dir);
    for path in &feature_paths {
        let feature = match Feature::parse_path(path, Default::default()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        evaluate_feature(&feature, &mut stats);
    }
    stats.report(&feature_paths);

    // floors: raise them as more scenarios pass, never lower them
    let floor = 69;
    assert!(
        stats.matched >= floor,
        "correctness coverage regressed: {} scenarios matched expected rows, floor is {}",
        stats.matched,
        floor
    );
    assert!(
        stats.mismatched == 0,
        "{} scenarios returned the wrong rows - a previously-correct \
         query now produces a different result",
        stats.mismatched
    );
    let exception_floor = 133;
    assert!(
        stats.exception_matched >= exception_floor,
        "exception-code coverage regressed: {} scenarios raised the \
         right ISO code, floor is {}",
        stats.exception_matched,
        exception_floor
    );
}

#[derive(Default)]
struct Stats {
    eligible: usize,
    executed: usize,
    /// Returned rows matched the expected table exactly.
    matched: usize,
    /// Returned rows differed from the expected table.
    mismatched: usize,
    /// Result held a value without a TCK literal form (Path, refs, temporals); excluded from match
    /// counts to keep the metric honest.
    incomparable: usize,
    /// Exception-expecting scenarios whose ISO code matched.
    exception_matched: usize,
    /// Exception-expecting scenarios with no error or a different ISO code.
    exception_mismatched: usize,
    skipped: usize,
    per_feature: Vec<FeatureResult>,
}

#[derive(Default)]
struct FeatureResult {
    rel_path: String,
    eligible: usize,
    matched: usize,
    mismatched: usize,
}

impl Stats {
    fn report(&self, feature_paths: &[PathBuf]) {
        eprintln!();
        eprintln!("--- opengql/tck result-correctness ----------------");
        eprintln!("features:        {}", feature_paths.len());
        eprintln!("eligible:        {}", self.eligible);
        eprintln!("executed:        {}", self.executed);
        eprintln!("matched rows:    {}", self.matched);
        eprintln!("mismatched rows: {}", self.mismatched);
        eprintln!("incomparable:    {}", self.incomparable);
        eprintln!("exception ok:    {}", self.exception_matched);
        eprintln!("exception miss:  {}", self.exception_mismatched);
        eprintln!("skipped:         {}", self.skipped);
        eprintln!();
        eprintln!(
            "  {:<58} {:>8} {:>8} {:>8}",
            "feature", "eligible", "match", "miss"
        );
        for r in &self.per_feature {
            eprintln!(
                "  {:<58} {:>8} {:>8} {:>8}",
                truncate(&r.rel_path, 58),
                r.eligible,
                r.matched,
                r.mismatched
            );
        }
        eprintln!("---------------------------------------------------");
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("...{}", &s[s.len() - max + 1..])
    }
}

fn collect_features(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("feature") {
            out.push(path);
        }
    }
}

fn evaluate_feature(feature: &Feature, stats: &mut Stats) {
    let mut local = FeatureResult {
        rel_path: rel_path_for(&feature.path),
        ..Default::default()
    };
    for scenario in &feature.scenarios {
        for expanded in expand_scenarios(scenario) {
            evaluate_scenario(&expanded, &mut local, stats);
        }
    }
    for rule in &feature.rules {
        for scenario in &rule.scenarios {
            for expanded in expand_scenarios(scenario) {
                evaluate_scenario(&expanded, &mut local, stats);
            }
        }
    }
    stats.per_feature.push(local);
}

fn evaluate_scenario(scenario: &Scenario, local: &mut FeatureResult, stats: &mut Stats) {
    let mut preamble: Vec<&Step> = Vec::new();
    let mut when_step: Option<&Step> = None;
    let mut then_step: Option<&Step> = None;
    let mut expected_exception_code: Option<String> = None;
    let mut needs_named_catalog = false;

    for step in &scenario.steps {
        let lc = step.value.to_lowercase();
        if lc.starts_with("having executed") {
            preamble.push(step);
        } else if lc.starts_with("executing query")
            || lc.starts_with("executing the query")
            || lc.starts_with("executing program")
            || lc.starts_with("executing the program")
        {
            when_step = Some(step);
        } else if lc.starts_with("the result should be") {
            then_step = Some(step);
        } else if lc.contains("catalog-") && lc.ends_with(" catalog") {
            needs_named_catalog = true;
        } else if lc.starts_with("an exception condition should be raised") {
            // step text: "an exception condition should be raised: 42000"
            if let Some(code) = step.value.split(':').nth(1) {
                expected_exception_code = Some(code.trim().to_string());
            }
        }
    }

    if needs_named_catalog {
        stats.skipped += 1;
        return;
    }

    // exception-expecting scenarios: run preamble + When, then assert the When errored with the
    // expected ISO code
    if let Some(expected_code) = expected_exception_code {
        evaluate_exception_scenario(scenario, preamble, when_step, expected_code, stats);
        return;
    }
    let Some(when_step) = when_step else {
        stats.skipped += 1;
        return;
    };
    let Some(then_step) = then_step else {
        stats.skipped += 1;
        return;
    };
    let Some(when_doc) = &when_step.docstring else {
        stats.skipped += 1;
        return;
    };

    // two `Then` forms: `the result should be: <table>` (row by row) and `the result should be
    // empty` (zero rows); the catalog scenarios use the latter, handled as an empty expected table
    let then_lc = then_step.value.to_lowercase();
    let expected = if let Some(table) = &then_step.table {
        canonicalise_expected_table(&table.rows)
    } else if then_lc.contains("should be empty") {
        Vec::new()
    } else {
        stats.skipped += 1;
        return;
    };

    stats.eligible += 1;
    local.eligible += 1;

    let in_any_order = then_lc.contains("in any order");

    // preamble
    let mut graph = Graph::new();
    let mut catalog = GqlCatalog::default();
    for step in preamble {
        let Some(doc) = &step.docstring else { continue };
        let q = doc.trim();
        if q.is_empty() {
            continue;
        }
        if !run_program(&mut graph, &mut catalog, q).is_ok() {
            return; // preamble failed: skip
        }
    }

    // When
    let outcome = run_program(&mut graph, &mut catalog, when_doc.trim());
    let StmtOutcome::Ran { rows, .. } = outcome else {
        return;
    };
    stats.executed += 1;

    // rows -> canonical strings
    let actual = match canonicalise_rows(&rows) {
        Some(rows) => rows,
        None => {
            stats.incomparable += 1;
            return;
        }
    };

    let (actual_norm, expected_norm) = if in_any_order {
        let mut a = actual.clone();
        a.sort();
        let mut e = expected.clone();
        e.sort();
        (a, e)
    } else {
        (actual, expected)
    };

    if actual_norm == expected_norm {
        stats.matched += 1;
        local.matched += 1;
    } else {
        stats.mismatched += 1;
        local.mismatched += 1;
    }
}

/// Evaluates a scenario whose `Then` expects an exception: preamble + When-query as on the row
/// path, then checks the outcome's ISO error code instead of rows.
fn evaluate_exception_scenario(
    _scenario: &Scenario,
    preamble: Vec<&Step>,
    when_step: Option<&Step>,
    expected_code: String,
    stats: &mut Stats,
) {
    let Some(when_step) = when_step else {
        stats.skipped += 1;
        return;
    };
    let Some(when_doc) = &when_step.docstring else {
        stats.skipped += 1;
        return;
    };

    let mut graph = Graph::new();
    let mut catalog = GqlCatalog::default();
    // the preamble must succeed; if it fails the scenario can't run cleanly, so skip instead of
    // blaming the When query
    for step in preamble {
        let Some(doc) = &step.docstring else { continue };
        let q = doc.trim();
        if q.is_empty() {
            continue;
        }
        if !run_program(&mut graph, &mut catalog, q).is_ok() {
            stats.skipped += 1;
            return;
        }
    }

    let outcome = run_program(&mut graph, &mut catalog, when_doc.trim());
    if outcome
        .iso_error_code()
        .map(|c| c == expected_code)
        .unwrap_or(false)
    {
        stats.exception_matched += 1;
    } else {
        stats.exception_mismatched += 1;
    }
}

/// Rows of `CypherValue` -> rows of trimmed strings; `None` if any cell has no TCK literal form.
fn canonicalise_rows(rows: &[Vec<CypherValue>]) -> Option<Vec<Vec<String>>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(canonicalise_value)
                .collect::<Option<Vec<_>>>()
        })
        .collect()
}

/// `CypherValue` -> TCK literal form.
fn canonicalise_value(v: &CypherValue) -> Option<String> {
    Some(match v {
        CypherValue::Null => "null".to_string(),
        CypherValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        CypherValue::Integer(i) => i.to_string(),
        CypherValue::Float(f) => format_float_tck(*f),
        CypherValue::String(s) => format!("'{}'", s),
        CypherValue::List(items) => {
            let parts: Option<Vec<_>> = items.iter().map(canonicalise_value).collect();
            format!("[{}]", parts?.join(", "))
        }
        CypherValue::Map(entries) => {
            let mut parts = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                parts.push(format!("{k}: {}", canonicalise_value(val)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        // nodes: `(:Label {prop: value, ...})`, labels chained (`(:A:B)`); the space before `{`
        // only follows rendered labels, so an unlabeled node with properties is `({prop: value})`,
        // not `( {prop: value})`
        CypherValue::Node {
            labels, properties, ..
        } => {
            let mut s = "(".to_string();
            for label in labels {
                s.push(':');
                s.push_str(label);
            }
            if !properties.is_empty() {
                if !labels.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format_property_map(properties)?);
            }
            s.push(')');
            s
        }
        // edges: `[:TYPE {prop: value, ...}]`; TCK literals never expose endpoint ids
        CypherValue::Edge {
            edge_type,
            properties,
            ..
        } => {
            let mut s = format!("[:{edge_type}");
            if !properties.is_empty() {
                s.push(' ');
                s.push_str(&format_property_map(properties)?);
            }
            s.push(']');
            s
        }
        // Path / NodeRef / EdgeRef / temporals: no canonical form, the scenario becomes
        // "incomparable"
        _ => return None,
    })
}

/// `PropertyMap` (BTreeMap<String, PropertyValue>) -> TCK map literal `{key: value, ...}`; keys in
/// the map's alphabetical order, so equivalent maps canonicalize identically.
fn format_property_map(props: &traverse_core::graph::property::PropertyMap) -> Option<String> {
    let mut parts = Vec::with_capacity(props.len());
    for (k, v) in props {
        parts.push(format!("{k}: {}", canonicalise_property_value(v)?));
    }
    Some(format!("{{{}}}", parts.join(", ")))
}

fn canonicalise_property_value(
    v: &traverse_core::graph::property::PropertyValue,
) -> Option<String> {
    use traverse_core::graph::property::PropertyValue;
    Some(match v {
        PropertyValue::Null => "null".to_string(),
        PropertyValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        PropertyValue::Int(i) => i.to_string(),
        PropertyValue::Float(f) => format_float_tck(*f),
        PropertyValue::String(s) => format!("'{}'", s),
        PropertyValue::List(items) => {
            let parts: Option<Vec<_>> = items.iter().map(canonicalise_property_value).collect();
            format!("[{}]", parts?.join(", "))
        }
        PropertyValue::Map(entries) => {
            let mut parts = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                parts.push(format!("{k}: {}", canonicalise_property_value(val)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        _ => return None,
    })
}

/// Float in TCK style: whole numbers as `1.0`, others with default Rust formatting.
fn format_float_tck(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Inf" } else { "-Inf" }.to_string();
    }
    if f == f.trunc() && f.abs() < 1e16 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

/// Expected-table data rows (header stripped), each cell trimmed. `gherkin::TableRow` has `.cells:
/// Vec<TableCell>` with `.value: String`.
fn canonicalise_expected_table(rows: &[Vec<String>]) -> Vec<Vec<String>> {
    rows.iter()
        .skip(1) // header row
        .map(|row| row.iter().map(|c| c.trim().to_string()).collect())
        .collect()
}

fn rel_path_for(path: &Option<PathBuf>) -> String {
    let Some(path) = path else {
        return "<unknown>".to_string();
    };
    let s = path.to_string_lossy();
    if let Some(idx) = s.find("features/") {
        s[idx + "features/".len()..].to_string()
    } else if let Some(idx) = s.find("features\\") {
        s[idx + "features\\".len()..].to_string()
    } else {
        s.into_owned()
    }
}
