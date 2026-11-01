//! Bulk Cypher importer: reads a `.cypher` file and writes a `.tvdb` file.
//!
//! Executes each line as a Cypher statement against an in-memory Graph,
//! then persists the result. No server required.
//!
//! Auto-indexes: when the first MATCH statement is seen (after CREATE-only
//! lines), automatically creates indexes on any `:Label {prop}` patterns
//! found in the preceding CREATEs. This avoids O(N) scans for edge creation.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Instant;

use indicatif::{ProgressBar, ProgressStyle};
use traverse_core::graph::Graph;
use traverse_core::storage::Database;

pub fn run_cypher_import(
    input: &str,
    output: &str,
    skip_errors: bool,
    pre_indexes: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let input_path = Path::new(input);

    // Use file size for progress (avoids slow line-counting pre-pass)
    let file_size = std::fs::metadata(input_path)?.len();
    eprintln!(
        "Importing {input} ({:.1} MB)",
        file_size as f64 / 1_048_576.0
    );

    let mut graph = Graph::new();

    // Execute any pre-index statements
    for idx_stmt in pre_indexes {
        eprintln!("Pre-index: {idx_stmt}");
        traverse_cypher::execute(&mut graph, idx_stmt)
            .map_err(|e| format!("pre-index failed: {e}\n  statement: {idx_stmt}"))?;
    }

    let file = std::fs::File::open(input_path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);

    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut executed: u64 = 0;
    let mut skipped: u64 = 0;
    let mut errors: u64 = 0;
    let mut bytes_read: u64 = 0;
    let mut seen_match = false;
    let mut auto_indexed = false;

    for line in reader.lines() {
        let line = line?;
        bytes_read += line.len() as u64 + 1; // +1 for newline
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with("//") {
            skipped += 1;
            pb.set_position(bytes_read);
            continue;
        }

        // Strip trailing semicolon (common in .cypher files)
        let query = trimmed.strip_suffix(';').unwrap_or(trimmed);

        // Auto-index: when we see the first MATCH after CREATE-only lines,
        // scan the graph for labels+properties and create indexes.
        if !seen_match && !auto_indexed && pre_indexes.is_empty() {
            let upper = query.to_ascii_uppercase();
            if upper.starts_with("MATCH") {
                seen_match = true;
                auto_indexed = true;
                auto_create_indexes(&mut graph, &pb);
            }
        }

        match traverse_cypher::execute(&mut graph, query) {
            Ok(_) => {
                executed += 1;
            }
            Err(e) => {
                errors += 1;
                if !skip_errors {
                    pb.finish_and_clear();
                    return Err(format!(
                        "line {}: {e}\n  query: {query}",
                        executed + errors + skipped
                    )
                    .into());
                }
                if errors <= 10 {
                    pb.println(format!(
                        "  error (line {}): {e}",
                        executed + errors + skipped
                    ));
                } else if errors == 11 {
                    pb.println("  (suppressing further error messages)");
                }
            }
        }

        if executed.is_multiple_of(10_000) {
            pb.set_position(bytes_read);
            pb.set_message(format!(
                "{executed} stmts, {} nodes, {} edges",
                graph.node_count(),
                graph.edge_count()
            ));
        }
    }

    pb.finish_and_clear();

    let nodes = graph.node_count();
    let edges = graph.edge_count();
    eprintln!("Executed {executed} statements: {nodes} nodes, {edges} edges");
    if errors > 0 {
        eprintln!("  ({errors} errors skipped)");
    }
    if skipped > 0 {
        eprintln!("  ({skipped} blank/comment lines skipped)");
    }

    // Save to .tvdb
    let out_path = Path::new(output);
    eprintln!("Saving to {output}...");
    Database::save(out_path, &graph, &[])?;

    let elapsed = start.elapsed();
    let rate = if elapsed.as_secs_f64() > 0.0 {
        executed as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    eprintln!("Done in {:.2}s ({:.0} stmt/s)", elapsed.as_secs_f64(), rate,);

    Ok(())
}

/// Auto-detect labels and their property keys from the graph, then create
/// indexes on each (label, property) pair. This makes subsequent MATCH lookups
/// use index scans instead of O(N) full scans.
fn auto_create_indexes(graph: &mut Graph, pb: &ProgressBar) {
    use std::collections::{BTreeSet, HashSet};
    use traverse_core::tx::view::GraphView;

    let strings = graph.strings();

    // Collect (label_name, property_key) pairs from existing nodes
    let mut label_props: HashSet<(String, String)> = HashSet::new();
    for node_id in <dyn GraphView>::nodes_vec(graph) {
        if let Ok(node) = graph.get_node(node_id) {
            for lid in &node.labels {
                let label = strings.resolve(lid.0).to_string();
                for kid in node.properties.keys() {
                    let prop = strings.resolve(kid.0).to_string();
                    label_props.insert((label.clone(), prop));
                }
            }
        }
    }

    // Sort for deterministic output
    let sorted: BTreeSet<_> = label_props.into_iter().collect();
    let count = sorted.len();
    if count == 0 {
        return;
    }

    pb.println(format!(
        "  Auto-creating {count} indexes for MATCH lookups..."
    ));
    let mut created = 0;
    for (label, prop) in &sorted {
        let idx_name = format!("auto_{}_{}", label.to_lowercase(), prop.to_lowercase());
        let stmt = format!("CREATE INDEX {idx_name} FOR (n:{label}) ON (n.{prop})");
        match traverse_cypher::execute(graph, &stmt) {
            Ok(_) => {
                created += 1;
            }
            Err(e) => {
                pb.println(format!("  index warning: {e}"));
            }
        }
    }
    pb.println(format!("  Created {created} indexes"));
}
