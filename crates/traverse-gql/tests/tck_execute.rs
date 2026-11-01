//! End-to-end TCK harness: parse -> lower -> **execute** every scenario whose
//! Given preamble a fresh in-memory graph can satisfy, asserting the executor
//! returns `Ok` without panicking. "Executable" coverage, as opposed to
//! `tck.rs`'s parse coverage: of the legal queries that parse and lower, how
//! many run on the real engine? That checks the bridge is wired right.
//!
//! Result rows aren't checked here (`tck_correctness.rs` does that); any `Ok`
//! passes, which catches panics, plan failures and lowering bugs that only show
//! at execution.
//!
//! Skipped:
//! * `Given catalog-N catalog` (needs a pre-built catalog fixture)
//! * scenarios without a `When executing query/program` step
//! * `Then an exception condition should be raised` (error codes are checked
//!   in `tck_correctness.rs`)

use std::path::{Path, PathBuf};

use gherkin::{Feature, Scenario, Step};
use traverse_core::graph::Graph;

mod common;
use common::{GqlCatalog, expand_scenarios, run_program};

fn tck_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/opengql_tck")
}

#[test]
fn opengql_tck_execute_coverage() {
    let root = tck_root();
    let features_dir = root.join("features");
    if !features_dir.exists() {
        eprintln!(
            "opengql/tck submodule not initialised at {}; skipping. \
             Run `git submodule update --init --recursive` to enable.",
            features_dir.display()
        );
        return;
    }

    let mut stats = Stats::default();
    let feature_paths = collect_features(&features_dir);
    assert!(
        !feature_paths.is_empty(),
        "expected feature files under {}",
        features_dir.display()
    );

    for path in &feature_paths {
        let feature = match Feature::parse_path(path, Default::default()) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("WARN: failed to parse {}: {e:?}", path.display());
                continue;
            }
        };
        evaluate_feature(&feature, &mut stats);
    }

    stats.report(&feature_paths);

    // floor: at least this many scenarios run end to end without an executor error; raise it as
    // coverage grows. The known misses are TCK inconsistencies (schemas/Create1.feature [7] is
    // named "if not exists" but omits `IF NOT EXISTS`, contradicting [3], which expects 42000 on a
    // duplicate `CREATE SCHEMA`).
    let floor = 70;
    assert!(
        stats.executed_ok >= floor,
        "executor coverage regressed: {} scenarios executed cleanly, floor is {}",
        stats.executed_ok,
        floor
    );
}

#[derive(Default)]
struct Stats {
    /// Scenarios that reached a When-query docstring (eligible).
    eligible: usize,
    /// Scenarios whose every preamble query lowered.
    preamble_ok: usize,
    /// Scenarios whose `When` query lowered.
    when_lowered: usize,
    /// Scenarios whose `When` query ran without error.
    executed_ok: usize,
    /// Skipped: named catalog Given, or exception-only Then.
    skipped: usize,
    per_feature: Vec<FeatureResult>,
}

#[derive(Default)]
struct FeatureResult {
    rel_path: String,
    eligible: usize,
    executed_ok: usize,
}

impl Stats {
    fn report(&self, feature_paths: &[PathBuf]) {
        eprintln!();
        eprintln!("--- opengql/tck execute-coverage ------------------");
        eprintln!("features:     {}", feature_paths.len());
        eprintln!("eligible:     {}", self.eligible);
        eprintln!("preamble ok:  {}", self.preamble_ok);
        eprintln!("when lower:   {}", self.when_lowered);
        eprintln!("executed ok:  {}", self.executed_ok);
        eprintln!("skipped:      {}", self.skipped);
        eprintln!();
        eprintln!("  {:<60} {:>8} {:>10}", "feature", "eligible", "executed");
        for r in &self.per_feature {
            eprintln!(
                "  {:<60} {:>8} {:>10}",
                truncate(&r.rel_path, 60),
                r.eligible,
                r.executed_ok
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
    // sort steps into preamble (Given-having-executed), the When-query and Then assertions; skip
    // scenarios outside the executable subset
    let mut preamble: Vec<&Step> = Vec::new();
    let mut when_query: Option<&Step> = None;
    let mut expects_exception = false;
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
            when_query = Some(step);
        } else if lc.contains("catalog-") && lc.ends_with(" catalog") {
            needs_named_catalog = true;
        } else if lc.starts_with("an exception condition should be raised") {
            expects_exception = true;
        }
    }

    let Some(when_step) = when_query else {
        stats.skipped += 1;
        return;
    };
    if needs_named_catalog {
        stats.skipped += 1;
        return;
    }
    if expects_exception {
        // exception-expecting scenarios: covered by tck_correctness.rs, skipped here so
        // "executed_ok" stays honest
        stats.skipped += 1;
        return;
    }

    stats.eligible += 1;
    local.eligible += 1;

    // fresh per-scenario state: graph for the cypher executor, catalog for GQL DDL
    let mut graph = Graph::new();
    let mut catalog = GqlCatalog::default();

    // preamble; any failure skips the scenario
    for step in preamble {
        let Some(doc) = &step.docstring else { continue };
        let q = doc.trim();
        if q.is_empty() {
            continue;
        }
        if !run_program(&mut graph, &mut catalog, q).is_ok() {
            return;
        }
    }
    stats.preamble_ok += 1;

    let Some(doc) = &when_step.docstring else {
        return;
    };
    let q = doc.trim();
    if q.is_empty() {
        return;
    }

    // `run_program` sends catalog statements to the in-memory catalog and queries to the cypher
    // executor; `when_lowered` counts runs that got past parse and lower
    let outcome = run_program(&mut graph, &mut catalog, q);
    if !matches!(
        outcome,
        common::StmtOutcome::ParseFailed | common::StmtOutcome::LowerFailed
    ) {
        stats.when_lowered += 1;
    }
    if outcome.is_ok() {
        stats.executed_ok += 1;
        local.executed_ok += 1;
    }
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
