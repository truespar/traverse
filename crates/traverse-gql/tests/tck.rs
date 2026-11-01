//! Parse-coverage harness for the opengql/tck submodule at
//! `tests/opengql_tck/`. Without it (a fresh clone without
//! `git submodule update --init`) the test is skipped, so `cargo test` still
//! passes.
//!
//! Each `.feature` file is parsed with the `gherkin` crate; every
//! `When executing query:` / `Given having executed the program:` docstring
//! (the GQL between `"""..."""`) goes through `traverse_gql::parse()` and
//! `traverse_gql::lower::lower()`. Pass/fail counters per category are printed
//! as a table at the end.
//!
//! Parse coverage asks "does the grammar accept every query the spec deems
//! legal?". Execution and result checks live in `tck_execute.rs` and
//! `tck_correctness.rs`.
//!
//! The test asserts floors on parse and lower coverage, so a parser regression
//! fails it. Raise the floors when coverage goes up.

use std::path::{Path, PathBuf};

use gherkin::{Feature, Step};
use traverse_gql::{lower, parse};

mod common;
use common::expand_scenarios;

/// Submodule root, relative to the manifest.
fn tck_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/opengql_tck")
}

#[test]
fn opengql_tck_parse_coverage() {
    let root = tck_root();
    let features_dir = root.join("features");
    if !features_dir.exists() {
        eprintln!(
            "opengql/tck submodule not initialised at {}; \
             skipping. Run `git submodule update --init --recursive` \
             to enable.",
            features_dir.display()
        );
        return;
    }

    let mut stats = Stats::default();
    let feature_paths = collect_features(&features_dir);
    assert!(
        !feature_paths.is_empty(),
        "expected at least one .feature file under {}",
        features_dir.display()
    );

    for path in &feature_paths {
        let feature = match Feature::parse_path(path, Default::default()) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("WARN: failed to parse {}: {e:?}", path.display());
                stats.feature_parse_errors += 1;
                continue;
            }
        };
        evaluate_feature(&feature, &mut stats);
    }

    stats.report(&feature_paths);

    // separate floors for parse and lower: catalog statements (CREATE/DROP SCHEMA, GRAPH, GRAPH
    // TYPE, transaction wrappers) parse but intentionally don't lower to Cypher; the harness (and
    // the server) dispatch them through a catalog instead
    let parse_floor = 210;
    let lower_floor = 166;
    assert!(
        stats.parsed >= parse_floor,
        "parse coverage regressed: {} queries parsed, floor is {}. \
         Investigate which grammar feature broke before lowering the floor.",
        stats.parsed,
        parse_floor
    );
    assert!(
        stats.lowered >= lower_floor,
        "lower coverage regressed: {} queries lowered, floor is {}. \
         A previously-lowered shape started failing.",
        stats.lowered,
        lower_floor
    );
}

#[derive(Default)]
struct Stats {
    feature_parse_errors: usize,
    /// GQL docstrings found in `When ... query:` / `Given ... program:` steps.
    extracted: usize,
    /// Of those, how many `traverse_gql::parse()` accepted.
    parsed: usize,
    /// Of the parsed, how many `lower()` translated cleanly.
    lowered: usize,
    /// Per-feature breakdown for the table.
    per_feature: Vec<FeatureResult>,
}

struct FeatureResult {
    /// Path relative to `features/`, for compact reporting.
    rel_path: String,
    extracted: usize,
    parsed: usize,
    lowered: usize,
}

impl Stats {
    fn report(&self, feature_paths: &[PathBuf]) {
        eprintln!();
        eprintln!("--- opengql/tck parse-coverage --------------------");
        eprintln!("features:       {}", feature_paths.len());
        if self.feature_parse_errors > 0 {
            eprintln!("feature read errors: {}", self.feature_parse_errors);
        }
        eprintln!(
            "queries seen:    {} (parsed: {}, lowered: {})",
            self.extracted, self.parsed, self.lowered
        );
        eprintln!();
        eprintln!(
            "  {:<60} {:>8} {:>8} {:>8}",
            "feature", "queries", "parsed", "lowered"
        );
        for r in &self.per_feature {
            eprintln!(
                "  {:<60} {:>8} {:>8} {:>8}",
                truncate(&r.rel_path, 60),
                r.extracted,
                r.parsed,
                r.lowered
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
        extracted: 0,
        parsed: 0,
        lowered: 0,
    };

    // background and scenario steps both count toward coverage
    if let Some(bg) = &feature.background {
        for step in &bg.steps {
            evaluate_step(step, &mut local);
        }
    }
    for scenario in &feature.scenarios {
        for expanded in expand_scenarios(scenario) {
            for step in &expanded.steps {
                evaluate_step(step, &mut local);
            }
        }
    }
    for rule in &feature.rules {
        for scenario in &rule.scenarios {
            for expanded in expand_scenarios(scenario) {
                for step in &expanded.steps {
                    evaluate_step(step, &mut local);
                }
            }
        }
    }

    stats.extracted += local.extracted;
    stats.parsed += local.parsed;
    stats.lowered += local.lowered;
    stats.per_feature.push(local);
}

fn evaluate_step(step: &Step, local: &mut FeatureResult) {
    // the two step phrases carrying a GQL body; the verb is matched case-insensitively so author
    // quirks ("And", "Given", "When") don't slip through
    let value = step.value.to_lowercase();
    let is_query_step = value.starts_with("executing query")
        || value.starts_with("executing the query")
        || value.starts_with("executing the program")
        || value.starts_with("executing program")
        || value.starts_with("having executed the program")
        || value.starts_with("having executed program");
    if !is_query_step {
        return;
    }
    let Some(docstring) = &step.docstring else {
        return;
    };
    let query = docstring.trim();
    if query.is_empty() {
        return;
    }
    local.extracted += 1;

    match parse(query) {
        Ok(program) => {
            local.parsed += 1;
            if lower::lower(&program).is_ok() {
                local.lowered += 1;
            }
        }
        Err(_errs) => {
            // parser rejected it; no per-diagnostic output (the floor guards regressions, the table
            // shows the gap)
        }
    }
}

fn rel_path_for(path: &Option<PathBuf>) -> String {
    let Some(path) = path else {
        return "<unknown>".to_string();
    };
    // strip through "features/" to keep rows compact
    let s = path.to_string_lossy();
    if let Some(idx) = s.find("features/") {
        s[idx + "features/".len()..].to_string()
    } else if let Some(idx) = s.find("features\\") {
        s[idx + "features\\".len()..].to_string()
    } else {
        s.into_owned()
    }
}
