//! Diagnostic harness: lists the **exact** scenarios failing at each TCK layer
//! (parse / lower / execute / correctness).
//!
//! ```text
//! cargo test -p traverse-gql --test tck_gaps -- --nocapture
//! ```
//!
//! Asserts nothing; logs each scenario's feature path, name and failing query
//! or reason. The floor-asserting harnesses are `tck.rs`, `tck_execute.rs` and
//! `tck_correctness.rs`.

use std::path::{Path, PathBuf};

use gherkin::{Feature, Scenario, Step};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;
use traverse_gql::{lower, parse};

mod common;
use common::{GqlCatalog, StmtOutcome, expand_scenarios, run_program};

fn tck_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/opengql_tck")
}

#[test]
fn opengql_tck_gaps() {
    let features_dir = tck_root().join("features");
    if !features_dir.exists() {
        eprintln!("opengql/tck submodule not initialised; skipping");
        return;
    }
    let feature_paths = collect_features(&features_dir);

    let mut unparsed: Vec<Gap> = Vec::new();
    let mut unlowered: Vec<Gap> = Vec::new();
    let mut exec_failed: Vec<ExecGap> = Vec::new();
    let mut row_mismatch: Vec<MismatchGap> = Vec::new();
    let mut row_incomparable: Vec<Gap> = Vec::new();
    let mut exc_mismatch: Vec<ExcGap> = Vec::new();

    for path in &feature_paths {
        let Ok(feature) = Feature::parse_path(path, Default::default()) else {
            continue;
        };
        let rel = rel_path_for(&feature.path);
        let mut scenarios: Vec<Scenario> = Vec::new();
        for s in &feature.scenarios {
            scenarios.extend(expand_scenarios(s));
        }
        for r in &feature.rules {
            for s in &r.scenarios {
                scenarios.extend(expand_scenarios(s));
            }
        }
        for scenario in &scenarios {
            evaluate_scenario(
                &rel,
                scenario,
                &mut unparsed,
                &mut unlowered,
                &mut exec_failed,
                &mut row_mismatch,
                &mut row_incomparable,
                &mut exc_mismatch,
            );
        }
    }

    print_section("UNPARSED - parser rejects the query outright", &unparsed);
    print_section(
        "UNLOWERED - parsed OK but lower() returned NotImplemented",
        &unlowered,
    );
    print_exec_section(
        "EXEC FAILED - lowered but engine/catalog errored",
        &exec_failed,
    );
    print_mismatch_section(
        "ROW MISMATCH - executed but returned wrong rows",
        &row_mismatch,
    );
    print_section(
        "ROW INCOMPARABLE - returned Path/temporal/etc, no canonical form yet",
        &row_incomparable,
    );
    print_exc_section(
        "EXCEPTION CODE MISMATCH - wrong ISO code or no error",
        &exc_mismatch,
    );

    eprintln!();
    eprintln!("== totals ==============================================");
    eprintln!("  unparsed:      {}", unparsed.len());
    eprintln!("  unlowered:     {}", unlowered.len());
    eprintln!("  exec_failed:   {}", exec_failed.len());
    eprintln!("  row_mismatch:  {}", row_mismatch.len());
    eprintln!("  incomparable:  {}", row_incomparable.len());
    eprintln!("  exc_mismatch:  {}", exc_mismatch.len());
    eprintln!("========================================================");
}

struct Gap {
    feature: String,
    scenario: String,
    query: String,
}

struct ExecGap {
    feature: String,
    scenario: String,
    query: String,
    reason: String,
}

struct MismatchGap {
    feature: String,
    scenario: String,
    query: String,
    expected: Vec<Vec<String>>,
    actual: Vec<Vec<String>>,
}

struct ExcGap {
    feature: String,
    scenario: String,
    query: String,
    expected_code: String,
    actual_code: Option<String>,
}

fn evaluate_scenario(
    feature: &str,
    scenario: &Scenario,
    unparsed: &mut Vec<Gap>,
    unlowered: &mut Vec<Gap>,
    exec_failed: &mut Vec<ExecGap>,
    row_mismatch: &mut Vec<MismatchGap>,
    row_incomparable: &mut Vec<Gap>,
    exc_mismatch: &mut Vec<ExcGap>,
) {
    let name = scenario.name.clone();
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
        } else if lc.starts_with("an exception condition should be raised")
            && let Some(code) = step.value.split(':').nth(1)
        {
            expected_exception_code = Some(code.trim().to_string());
        }
    }

    if needs_named_catalog {
        return;
    }
    let Some(when_step) = when_step else { return };
    let Some(when_doc) = &when_step.docstring else {
        return;
    };
    let query = when_doc.trim().to_string();
    if query.is_empty() {
        return;
    }

    // layer 1: parse + lower of the When query
    match parse(&query) {
        Err(_) => {
            unparsed.push(Gap {
                feature: feature.to_string(),
                scenario: name.clone(),
                query: query.clone(),
            });
            return;
        }
        Ok(prog) => {
            if lower::lower(&prog).is_err() {
                // catalog statements don't go through lowering, so their lower failures are
                // expected; only a program with NO catalog statements counts as a lower gap
                let only_catalog = prog.statements.iter().all(is_catalog_stmt);
                if !only_catalog {
                    unlowered.push(Gap {
                        feature: feature.to_string(),
                        scenario: name.clone(),
                        query: query.clone(),
                    });
                }
            }
        }
    }

    // layer 2: execute preamble + When through run_program
    let mut graph = Graph::new();
    let mut catalog = GqlCatalog::default();
    let mut preamble_failed = false;
    for step in &preamble {
        let Some(doc) = &step.docstring else { continue };
        let q = doc.trim();
        if q.is_empty() {
            continue;
        }
        let out = run_program(&mut graph, &mut catalog, q);
        if !out.is_ok() {
            preamble_failed = true;
            break;
        }
    }
    if preamble_failed {
        // don't double-count; the preamble issue surfaces in its own scenario
        return;
    }

    let outcome = run_program(&mut graph, &mut catalog, &query);

    // exception scenarios: check the ISO code
    if let Some(expected_code) = &expected_exception_code {
        let actual_code = outcome.iso_error_code().map(|s| s.to_string());
        if actual_code.as_deref() != Some(expected_code.as_str()) {
            exc_mismatch.push(ExcGap {
                feature: feature.to_string(),
                scenario: name.clone(),
                query: query.clone(),
                expected_code: expected_code.clone(),
                actual_code,
            });
        }
        return;
    }

    // otherwise the outcome must be Ran
    let StmtOutcome::Ran { rows, .. } = outcome else {
        let reason = match &outcome {
            StmtOutcome::ParseFailed => "ParseFailed (after preamble)".to_string(),
            StmtOutcome::LowerFailed => "LowerFailed".to_string(),
            StmtOutcome::CatalogError(e) => format!("CatalogError: {e:?}"),
            StmtOutcome::EngineError(e) => format!("EngineError: {e}"),
            StmtOutcome::Ran { .. } => unreachable!(),
        };
        exec_failed.push(ExecGap {
            feature: feature.to_string(),
            scenario: name.clone(),
            query: query.clone(),
            reason,
        });
        return;
    };

    // layer 3: row comparison (only when Then has a table)
    let Some(then_step) = then_step else { return };
    let then_lc = then_step.value.to_lowercase();
    let expected = if let Some(table) = &then_step.table {
        canonicalise_expected_table(&table.rows)
    } else if then_lc.contains("should be empty") {
        Vec::new()
    } else {
        return;
    };

    let actual = match canonicalise_rows(&rows) {
        Some(r) => r,
        None => {
            row_incomparable.push(Gap {
                feature: feature.to_string(),
                scenario: name.clone(),
                query: query.clone(),
            });
            return;
        }
    };
    let in_any_order = then_lc.contains("in any order");
    let (a, e) = if in_any_order {
        let mut a = actual.clone();
        a.sort();
        let mut e = expected.clone();
        e.sort();
        (a, e)
    } else {
        (actual, expected)
    };
    if a != e {
        row_mismatch.push(MismatchGap {
            feature: feature.to_string(),
            scenario: name.clone(),
            query: query.clone(),
            expected: e,
            actual: a,
        });
    }
}

fn is_catalog_stmt(s: &traverse_gql::Statement) -> bool {
    use traverse_gql::Statement::*;
    matches!(
        s,
        CreateSchema { .. }
            | DropSchema { .. }
            | CreateGraph { .. }
            | DropGraph { .. }
            | CreateGraphType { .. }
            | DropGraphType { .. }
            | Use { .. }
            | Transaction { .. }
    )
}

fn print_section(title: &str, gaps: &[Gap]) {
    eprintln!();
    eprintln!("---- {title} ({} entries) ----", gaps.len());
    for (i, g) in gaps.iter().enumerate() {
        eprintln!(
            "{i:>3}. {}  ::  {}",
            short(&g.feature, 40),
            short(&g.scenario, 60)
        );
        for line in g.query.lines().take(8) {
            eprintln!("      {line}");
        }
        if g.query.lines().count() > 8 {
            eprintln!("      ...");
        }
    }
}

fn print_exec_section(title: &str, gaps: &[ExecGap]) {
    eprintln!();
    eprintln!("---- {title} ({} entries) ----", gaps.len());
    for (i, g) in gaps.iter().enumerate() {
        eprintln!(
            "{i:>3}. {}  ::  {}",
            short(&g.feature, 40),
            short(&g.scenario, 60)
        );
        eprintln!("      reason: {}", g.reason);
        for line in g.query.lines().take(6) {
            eprintln!("      {line}");
        }
        if g.query.lines().count() > 6 {
            eprintln!("      ...");
        }
    }
}

fn print_mismatch_section(title: &str, gaps: &[MismatchGap]) {
    eprintln!();
    eprintln!("---- {title} ({} entries) ----", gaps.len());
    for (i, g) in gaps.iter().enumerate() {
        eprintln!(
            "{i:>3}. {}  ::  {}",
            short(&g.feature, 40),
            short(&g.scenario, 60)
        );
        for line in g.query.lines().take(4) {
            eprintln!("      {line}");
        }
        eprintln!("      expected:");
        for row in &g.expected {
            eprintln!("        {row:?}");
        }
        eprintln!("      actual:");
        for row in &g.actual {
            eprintln!("        {row:?}");
        }
    }
}

fn print_exc_section(title: &str, gaps: &[ExcGap]) {
    eprintln!();
    eprintln!("---- {title} ({} entries) ----", gaps.len());
    for (i, g) in gaps.iter().enumerate() {
        eprintln!(
            "{i:>3}. {}  ::  {}",
            short(&g.feature, 40),
            short(&g.scenario, 60)
        );
        eprintln!(
            "      expected: {}    actual: {}",
            g.expected_code,
            g.actual_code.as_deref().unwrap_or("<no error>")
        );
        for line in g.query.lines().take(4) {
            eprintln!("      {line}");
        }
    }
}

fn short(s: &str, max: usize) -> String {
    if s.len() <= max {
        format!("{s:<max$}")
    } else {
        format!("...{}", &s[s.len() - max + 1..])
    }
}

fn canonicalise_rows(rows: &[Vec<CypherValue>]) -> Option<Vec<Vec<String>>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(canonicalise_value)
                .collect::<Option<Vec<_>>>()
        })
        .collect()
}

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
        _ => return None,
    })
}

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
        _ => return None,
    })
}

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

fn canonicalise_expected_table(rows: &[Vec<String>]) -> Vec<Vec<String>> {
    rows.iter()
        .skip(1)
        .map(|row| row.iter().map(|c| c.trim().to_string()).collect())
        .collect()
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
