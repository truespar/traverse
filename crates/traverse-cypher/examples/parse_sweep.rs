//! Parse-checks a JSON list of `{page, query}` objects against the openCypher
//! parser, e.g. the example queries from the Neo4j Cypher manual. Prints one
//! line per FAILING query; always exits 0 (the caller aggregates).
//!
//!     cargo run --example parse_sweep -- manual_examples.json

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: parse_sweep <file.json>");
    let data = std::fs::read_to_string(&path).expect("read file");
    let items: serde_json::Value = serde_json::from_str(&data).expect("parse json");
    let mut total = 0usize;
    let mut failed = 0usize;
    for item in items.as_array().expect("array") {
        let page = item["page"].as_str().unwrap_or("?");
        let query = item["query"].as_str().unwrap_or("");
        total += 1;
        if let Err(e) = traverse_cypher::parse(query) {
            failed += 1;
            let first = query.lines().next().unwrap_or("");
            let msg = format!("{e}");
            let msg_line = msg.lines().next().unwrap_or("");
            println!("FAIL\t{page}\t{first}\t{msg_line}");
        }
    }
    eprintln!(
        "parse sweep: {total} queries, {} ok, {failed} failed",
        total - failed
    );
}
