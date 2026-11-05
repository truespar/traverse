//! Ad-hoc query runner for engine work: reads a file of statements (one per line, blank lines
//! and `#` comments skipped); every statement but the last is setup, the last is the query,
//! whose rows and plan are printed.

use traverse_core::graph::Graph;

fn main() {
    let path = std::env::args().nth(1).expect("a statements file");
    let big = std::env::args().nth(2).is_some();
    if big {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || run(&path))
            .unwrap()
            .join()
            .unwrap();
    } else {
        run(&path);
    }
}

fn run(path: &str) {
    let text = std::fs::read_to_string(path).expect("readable statements file");
    let stmts: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut g = Graph::new();
    let (setup, query) = stmts.split_at(stmts.len().saturating_sub(1));
    for s in setup {
        if let Err(e) = traverse_cypher::execute(&mut g, s) {
            eprintln!("setup failed: {e}\n  {s}");
            std::process::exit(1);
        }
    }
    let Some(q) = query.first() else { return };
    match traverse_cypher::execute(&mut g, &format!("EXPLAIN {q}")) {
        Ok(r) => eprintln!("plan: {:#?}", r.plan),
        Err(e) => eprintln!("explain failed: {e}"),
    }
    match traverse_cypher::execute(&mut g, q) {
        Ok(r) => {
            println!("columns: {:?}", r.columns);
            for row in &r.rows {
                println!("{row:?}");
            }
            println!("{} rows, stats {:?}", r.rows.len(), r.stats);
        }
        Err(e) => println!("error: {e}"),
    }
}
