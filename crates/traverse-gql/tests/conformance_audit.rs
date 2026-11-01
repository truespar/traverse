//! Conformance audit: treats `CONFORMANCE.md` as executable documentation.
//! Parses the row tables, takes each `Yes` row's evidence column and checks the
//! referenced test files exist under `tests/`, catching *documentation drift*
//! (a row citing a renamed or deleted test file).
//!
//! Checks:
//! * the row tables parse (catches malformed markdown) and every status is one of the legend's;
//! * the advertised counts match the row totals;
//! * every test file in an evidence column exists;
//! * each referenced file has at least one test.
//!
//! Not checked: that those tests exercise the claimed feature (their own job),
//! or that code references in a row (functions, AST variants) exist.

use std::fs;
use std::path::PathBuf;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_conformance() -> String {
    let p = crate_root().join("CONFORMANCE.md");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("could not read {}: {e}", p.display()))
}

/// A row's status column, one of the legend's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Yes,
    Partial,
    No,
    Deferred,
}

impl Status {
    fn parse(cell: &str) -> Option<Self> {
        match cell {
            "Yes" => Some(Self::Yes),
            "Partial" => Some(Self::Partial),
            "No" => Some(Self::No),
            "Deferred" => Some(Self::Deferred),
            _ => None,
        }
    }
}

/// One conformance-table row; the raw evidence column is kept for extracting test files.
#[derive(Debug)]
struct Row {
    id: String,
    /// `None` when the cell isn't one of the legend's words (kept raw in `status_cell`)
    status: Option<Status>,
    status_cell: String,
    evidence: String,
}

/// Parses `CONFORMANCE.md`'s two row tables (the mandatory and optional sections): rows shaped
/// `| <id> | <desc> | <status> | <evidence> |`.
fn parse_rows(md: &str) -> Vec<Row> {
    let mut out = Vec::new();
    let mut in_rows = false;
    for line in md.lines() {
        // the row tables live under "## Mandatory features" and "## Optional features"; the
        // legend before them and the summary after them are tables of another shape
        if let Some(heading) = line.strip_prefix("## ") {
            in_rows = heading.starts_with("Mandatory features")
                || heading.starts_with("Optional features");
            continue;
        }
        let trimmed = line.trim_start();
        if !in_rows || !trimmed.starts_with('|') {
            continue;
        }
        // honor the `\|` escape (some cells contain literal pipes, e.g. row 20.23's `(`\|\|`)`):
        // swap to a sentinel before splitting, restore per cell
        let sentinel = "\u{FFFD}";
        let working = trimmed.replace("\\|", sentinel);
        let cells: Vec<String> = working
            .split('|')
            .map(|c| c.trim().replace(sentinel, "|"))
            .collect();
        let cells: Vec<&str> = cells.iter().map(|s| s.as_str()).collect();
        // `| a | b | c | d |` -> ["", a, b, c, d, ""], 6 entries
        if cells.len() < 5 {
            continue;
        }
        let id = cells[1].to_string();
        if id.is_empty() || id.starts_with("---") || id == "Subclause" || id == "ID" {
            continue;
        }
        // skip summary rows (bold first cell)
        if id.starts_with("**") {
            continue;
        }
        out.push(Row {
            id,
            status: Status::parse(cells[3]),
            status_cell: cells[3].to_string(),
            evidence: cells[4].to_string(),
        });
    }
    out
}

/// `tests/<filename>.rs` paths in a row's evidence column; forward slashes by convention,
/// backslashes accepted too.
fn extract_test_paths(evidence: &str) -> Vec<String> {
    let mut paths = Vec::new();
    // manual scan for `tests/` ... `.rs`; no regex crate for one tiny scan
    let bytes = evidence.as_bytes();
    let mut i = 0;
    while i + 6 < bytes.len() {
        let head = &bytes[i..i + 6];
        if head == b"tests/" {
            let mut j = i + 6;
            while j < bytes.len() {
                let ch = bytes[j];
                if ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'.' || ch == b'/' {
                    j += 1;
                } else {
                    break;
                }
            }
            let candidate = &evidence[i..j];
            if candidate.ends_with(".rs") {
                paths.push(candidate.to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    paths
}

/// `#[test]` count in a Rust source file (best-effort, regex-free); at least 1 required.
fn count_test_attrs(path: &PathBuf) -> usize {
    let Ok(src) = fs::read_to_string(path) else {
        return 0;
    };
    let mut count = 0;
    for line in src.lines() {
        // `#[test]` or `#[test(...)]`, leading whitespace ignored
        let trimmed = line.trim_start();
        if trimmed == "#[test]" || trimmed.starts_with("#[test(") {
            count += 1;
        }
    }
    count
}

#[test]
fn conformance_md_parses_to_100_rows() {
    let md = read_conformance();
    let rows = parse_rows(&md);
    if rows.len() != 100 {
        // dump the IDs found so a missing (or extra) row is obvious
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        panic!(
            "expected 100 rows total (40 mandatory + 60 optional); got {}\n\
             extracted ids: {:#?}",
            rows.len(),
            ids,
        );
    }
}

#[test]
fn no_row_has_unrecognised_status() {
    let md = read_conformance();
    let rows = parse_rows(&md);
    for r in &rows {
        assert!(
            r.status.is_some(),
            "row {:?} has status {:?}; the legend's are Yes, Partial, No, Deferred",
            r.id,
            r.status_cell,
        );
    }
}

#[test]
fn ticked_rows_reference_existing_test_files() {
    let md = read_conformance();
    let rows = parse_rows(&md);
    let mut missing: Vec<(String, String)> = Vec::new();
    for r in &rows {
        // only fully-claimed rows; partial / non-goal / deferred rows may point to docs instead
        if r.status != Some(Status::Yes) {
            continue;
        }
        for rel in extract_test_paths(&r.evidence) {
            let abs = crate_root().join(&rel);
            if !abs.is_file() {
                missing.push((r.id.clone(), rel));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "rows reference test files that don't exist:\n{}",
        missing
            .iter()
            .map(|(id, p)| format!("  {id}: {p}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

#[test]
fn referenced_test_files_have_at_least_one_test() {
    let md = read_conformance();
    let rows = parse_rows(&md);
    let mut empty: Vec<(String, String)> = Vec::new();
    for r in &rows {
        if r.status != Some(Status::Yes) {
            continue;
        }
        for rel in extract_test_paths(&r.evidence) {
            let abs = crate_root().join(&rel);
            if !abs.is_file() {
                // reported by the previous test
                continue;
            }
            if count_test_attrs(&abs) == 0 {
                empty.push((r.id.clone(), rel));
            }
        }
    }
    assert!(
        empty.is_empty(),
        "rows reference test files with no #[test] functions:\n{}",
        empty
            .iter()
            .map(|(id, p)| format!("  {id}: {p}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

#[test]
fn summary_table_matches_row_status_counts() {
    let md = read_conformance();
    let rows = parse_rows(&md);
    let mut ticked = 0usize;
    let mut partial = 0usize;
    let mut unsupported = 0usize;
    let mut deferred = 0usize;
    for r in &rows {
        match r.status {
            Some(Status::Yes) => ticked += 1,
            Some(Status::Partial) => partial += 1,
            Some(Status::No) => unsupported += 1,
            Some(Status::Deferred) => deferred += 1,
            None => {}
        }
    }
    // the summary table's Total row must match the row totals; a mismatch means the doc and table
    // drifted or new rows need the summary updated
    let total = md
        .lines()
        .find(|l| l.trim_start().starts_with("| **Total**"))
        .expect("CONFORMANCE.md has no `| **Total** |` summary row");
    let counts: Vec<usize> = total
        .split('|')
        .map(|c| c.trim().trim_matches('*'))
        .filter(|c| !c.is_empty())
        .skip(1) // the "Total" label
        .map(|c| {
            c.parse()
                .unwrap_or_else(|_| panic!("bad count {c:?} in {total}"))
        })
        .collect();
    assert_eq!(
        counts,
        vec![ticked, partial, unsupported, deferred, rows.len()],
        "CONFORMANCE.md summary disagrees with the rows: \
         actual (Yes {ticked}, Partial {partial}, No {unsupported}, Deferred {deferred}, {} rows)",
        rows.len()
    );
}
