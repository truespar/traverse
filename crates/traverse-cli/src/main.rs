mod bolt_client;
mod display;
mod helper;
mod highlight;
mod util;

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal};

use clap::{CommandFactory, Parser};
use traverse_cypher::types::CypherValue;

use crate::bolt_client::{BoltClient, QueryResult};
use crate::display::{OutputFormat, display_error, display_error_with_position, display_results};
use crate::helper::CypherHelper;
use crate::util::{parse_param_value, split_statements};

/// Traverse interactive Cypher shell
#[derive(Parser)]
#[command(
    name = "traverse-cli",
    about,
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", "Copyright (c) 2026 The Intelligence Company AB (publ). MIT OR Apache-2.0."),
    before_help = concat!("traverse-cli ", env!("CARGO_PKG_VERSION")),
)]
struct Cli {
    /// Bolt URI (bolt://host:port or host:port)
    #[arg(short, long, default_value = "bolt://localhost:7690")]
    bolt: String,

    /// Open .tvdb file directly (embedded mode, no server needed)
    #[arg(short, long)]
    file: Option<String>,

    /// Run a .cypher script file
    #[arg(short, long)]
    script: Option<String>,

    /// Username for authentication
    #[arg(short, long)]
    user: Option<String>,

    /// Password for authentication
    #[arg(short, long)]
    password: Option<String>,

    /// Output format: table, json, csv
    #[arg(long, default_value = "table")]
    format: String,

    /// Select a database (Bolt mode only)
    #[arg(short, long)]
    database: Option<String>,

    /// Disable syntax highlighting
    #[arg(long)]
    no_highlight: bool,

    /// Skip TLS certificate verification (for self-signed certs)
    #[arg(long)]
    tls_insecure: bool,

    /// Query to execute (non-interactive)
    query: Option<String>,
}

/// Trait abstracting over Bolt and embedded backends.
trait Backend {
    fn execute(
        &mut self,
        query: &str,
        params: &HashMap<String, CypherValue>,
    ) -> Result<QueryResult, String>;
    fn begin_tx(&mut self) -> Result<(), String>;
    fn commit_tx(&mut self) -> Result<(), String>;
    fn rollback_tx(&mut self) -> Result<(), String>;
    fn close(&mut self) {}

    /// Switch to a different database.
    fn set_database(&mut self, db: &str) -> Result<(), String>;

    /// Return the current database name, if known.
    fn current_database(&self) -> Option<&str>;

    /// Return a description of the connection for the banner.
    fn connection_info(&self) -> String;

    /// Fetch schema words (labels, relationship types, property keys) for tab completion.
    fn fetch_schema_words(&mut self) -> Vec<String> {
        let mut words = Vec::new();
        let empty = HashMap::new();
        for query in &[
            "CALL db.labels()",
            "CALL db.relationshipTypes()",
            "CALL db.propertyKeys()",
        ] {
            if let Ok(result) = self.execute(query, &empty) {
                for row in &result.rows {
                    if let Some(CypherValue::String(s)) = row.first() {
                        words.push(s.clone());
                    }
                }
            }
        }
        words
    }

    /// Fetch database stats (node count, edge count) for the banner.
    fn fetch_db_stats(&mut self) -> (i64, i64) {
        let empty = HashMap::new();
        let nodes = self
            .execute("MATCH (n) RETURN count(n) AS c", &empty)
            .ok()
            .and_then(|r| r.rows.first()?.first()?.as_integer())
            .unwrap_or(0);
        let edges = self
            .execute("MATCH ()-[r]->() RETURN count(r) AS c", &empty)
            .ok()
            .and_then(|r| r.rows.first()?.first()?.as_integer())
            .unwrap_or(0);
        (nodes, edges)
    }
}

/// Embedded mode backend using direct Database access.
struct EmbeddedBackend {
    db: traverse_core::storage::database::Database,
    path: String,
}

impl Backend for EmbeddedBackend {
    fn set_database(&mut self, _db: &str) -> Result<(), String> {
        Err("Database switching is not supported in embedded mode".into())
    }

    fn current_database(&self) -> Option<&str> {
        None
    }

    fn execute(
        &mut self,
        query: &str,
        params: &HashMap<String, CypherValue>,
    ) -> Result<QueryResult, String> {
        let is_write = traverse_server::is_write_query(query);
        if is_write {
            let mut tx = self.db.begin_write();
            let result = if params.is_empty() {
                traverse_cypher::execute(&mut tx, query)
            } else {
                traverse_cypher::execute_with_params(&mut tx, query, params)
            }
            .map_err(|e| e.to_string())?;

            tx.commit().map_err(|e| e.to_string())?;
            self.db.flush().map_err(|e| e.to_string())?;
            Ok(cypher_result_to_query_result(result))
        } else {
            let mut tx = self.db.begin_read().map_err(|e| e.to_string())?;
            let result = if params.is_empty() {
                traverse_cypher::execute(&mut tx, query)
            } else {
                traverse_cypher::execute_with_params(&mut tx, query, params)
            }
            .map_err(|e| e.to_string())?;
            Ok(cypher_result_to_query_result(result))
        }
    }

    fn begin_tx(&mut self) -> Result<(), String> {
        Err("Explicit transactions not supported in embedded mode (each query auto-commits)".into())
    }

    fn commit_tx(&mut self) -> Result<(), String> {
        Err("No active transaction".into())
    }

    fn rollback_tx(&mut self) -> Result<(), String> {
        Err("No active transaction".into())
    }

    fn connection_info(&self) -> String {
        format!("embedded: {}", self.path)
    }

    fn fetch_schema_words(&mut self) -> Vec<String> {
        use std::collections::BTreeSet;
        use traverse_core::tx::GraphView;

        let tx = self.db.begin_read().unwrap();
        let strings = GraphView::strings(&tx);
        let mut words = BTreeSet::new();

        // Labels
        for nid in GraphView::nodes_vec(&tx) {
            if let Ok(node) = GraphView::get_node(&tx, nid) {
                for lid in &node.labels {
                    words.insert(strings.resolve(lid.0).to_string());
                }
                for k in node.properties.keys() {
                    words.insert(strings.resolve(k.0).to_string());
                }
            }
        }

        // Relationship types + edge properties
        for eid in GraphView::all_edges_vec(&tx) {
            if let Ok(edge) = GraphView::get_edge(&tx, eid) {
                words.insert(strings.resolve(edge.edge_type.0).to_string());
                for k in edge.properties.keys() {
                    words.insert(strings.resolve(k.0).to_string());
                }
            }
        }

        words.into_iter().collect()
    }
}

/// Bolt mode backend wrapping the async BoltClient.
struct BoltBackend {
    client: BoltClient,
    rt: tokio::runtime::Handle,
    addr: String,
    tls: bool,
    database: Option<String>,
}

impl Backend for BoltBackend {
    fn set_database(&mut self, db: &str) -> Result<(), String> {
        self.database = Some(db.to_string());
        self.client.database = Some(db.to_string());
        Ok(())
    }

    fn current_database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    fn execute(
        &mut self,
        query: &str,
        params: &HashMap<String, CypherValue>,
    ) -> Result<QueryResult, String> {
        let result = self
            .rt
            .block_on(self.client.run_query(query, params.clone()))
            .map_err(|e| e.to_string())?;

        // Auto-detect default database from first query response
        if self.database.is_none()
            && let Some(ref db) = result.db
        {
            self.database = Some(db.clone());
            self.client.database = Some(db.clone());
        }

        Ok(result)
    }

    fn begin_tx(&mut self) -> Result<(), String> {
        self.rt
            .block_on(self.client.begin_tx())
            .map_err(|e| e.to_string())
    }

    fn commit_tx(&mut self) -> Result<(), String> {
        self.rt
            .block_on(self.client.commit_tx())
            .map_err(|e| e.to_string())
    }

    fn rollback_tx(&mut self) -> Result<(), String> {
        self.rt
            .block_on(self.client.rollback_tx())
            .map_err(|e| e.to_string())
    }

    fn close(&mut self) {
        let _ = self.rt.block_on(self.client.goodbye());
    }

    fn connection_info(&self) -> String {
        if self.tls {
            format!("bolt+s://{}", self.addr)
        } else {
            format!("bolt://{}", self.addr)
        }
    }
}

fn cypher_result_to_query_result(result: traverse_cypher::executor::CypherResult) -> QueryResult {
    use traverse_server::packstream::Value;

    let stats: Vec<(String, Value)> = {
        let s = &result.stats;
        let mut v = Vec::new();
        if s.nodes_created > 0 {
            v.push(("nodes_created".into(), Value::Integer(s.nodes_created)));
        }
        if s.nodes_deleted > 0 {
            v.push(("nodes_deleted".into(), Value::Integer(s.nodes_deleted)));
        }
        if s.relationships_created > 0 {
            v.push((
                "relationships_created".into(),
                Value::Integer(s.relationships_created),
            ));
        }
        if s.relationships_deleted > 0 {
            v.push((
                "relationships_deleted".into(),
                Value::Integer(s.relationships_deleted),
            ));
        }
        if s.properties_set > 0 {
            v.push(("properties_set".into(), Value::Integer(s.properties_set)));
        }
        if s.labels_added > 0 {
            v.push(("labels_added".into(), Value::Integer(s.labels_added)));
        }
        if s.labels_removed > 0 {
            v.push(("labels_removed".into(), Value::Integer(s.labels_removed)));
        }
        if s.indexes_added > 0 {
            v.push(("indexes_added".into(), Value::Integer(s.indexes_added)));
        }
        if s.indexes_removed > 0 {
            v.push(("indexes_removed".into(), Value::Integer(s.indexes_removed)));
        }
        v
    };

    QueryResult {
        columns: result.columns,
        rows: result.rows,
        stats,
        plan: result.plan,
        db: None,
    }
}

fn main() {
    // No arguments -> show help (same as --help) and exit
    if std::env::args().len() <= 1 {
        Cli::command().print_help().ok();
        return;
    }

    let cli = Cli::parse();

    let format = OutputFormat::parse(&cli.format).unwrap_or_else(|| {
        eprintln!(
            "Unknown format '{}', using table. Valid: table, json, csv",
            cli.format
        );
        OutputFormat::Table
    });

    // Build the tokio runtime (needed for Bolt mode)
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    // Connect to backend
    let mut backend: Box<dyn Backend> = if let Some(ref path) = cli.file {
        // Embedded mode
        let db = traverse_core::storage::database::Database::open(path).unwrap_or_else(|e| {
            eprintln!("Failed to open database '{}': {}", path, e);
            std::process::exit(1);
        });

        Box::new(EmbeddedBackend {
            db,
            path: path.clone(),
        })
    } else {
        // Bolt mode - strip bolt:// or bolt+s:// prefix, detect TLS
        let (addr, tls) = if let Some(rest) = cli.bolt.strip_prefix("bolt+s://") {
            (rest.to_string(), true)
        } else if let Some(rest) = cli.bolt.strip_prefix("bolt://") {
            (rest.to_string(), false)
        } else {
            (cli.bolt.clone(), false)
        };
        let client = rt.block_on(async {
            BoltClient::connect(
                &addr,
                cli.user.as_deref(),
                cli.password.as_deref(),
                tls,
                cli.tls_insecure,
            )
            .await
        });
        match client {
            Ok(mut client) => {
                // Apply --database flag
                if let Some(ref db) = cli.database {
                    client.database = Some(db.clone());
                }
                Box::new(BoltBackend {
                    client,
                    rt: rt.handle().clone(),
                    addr,
                    tls,
                    database: cli.database.clone(),
                })
            }
            Err(e) => {
                eprintln!("Failed to connect to {}: {}", cli.bolt, e);
                std::process::exit(1);
            }
        }
    };

    // Check for non-interactive mode
    let stdin_is_tty = std::io::stdin().is_terminal();
    let empty_params = HashMap::new();

    // --script flag: run a .cypher file
    // non-interactive runs exit 1 when a statement fails, so scripts can tell
    if let Some(ref script_path) = cli.script {
        let ok = run_script_file(&mut *backend, script_path, &empty_params, format);
        backend.close();
        exit_unless(ok);
        return;
    }

    if let Some(ref query) = cli.query {
        // Positional query argument
        let ok = execute_statements(&mut *backend, query, &empty_params, format);
        backend.close();
        exit_unless(ok);
        return;
    }

    if !stdin_is_tty {
        // Piped stdin
        let stdin = std::io::stdin();
        let mut buf = String::new();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => buf.push_str(&l),
                Err(e) => {
                    display_error(&format!("Failed to read stdin: {e}"));
                    std::process::exit(1);
                }
            }
            buf.push('\n');
        }
        let query = buf.trim();
        let ok =
            query.is_empty() || execute_statements(&mut *backend, query, &empty_params, format);
        backend.close();
        exit_unless(ok);
        return;
    }

    // Interactive REPL
    run_repl(&mut *backend, format, !cli.no_highlight);
    backend.close();
}

/// Exits with status 1 after a failed non-interactive run.
fn exit_unless(ok: bool) {
    if !ok {
        std::process::exit(1);
    }
}

/// Execute multiple semicolon-separated statements, stopping on first error. `false` if one failed.
fn execute_statements(
    backend: &mut dyn Backend,
    input: &str,
    params: &HashMap<String, CypherValue>,
    format: OutputFormat,
) -> bool {
    split_statements(input)
        .into_iter()
        .all(|stmt| execute_single(backend, stmt, params, format))
}

/// Execute a single statement. Returns false if execution failed (to stop multi-statement).
fn execute_single(
    backend: &mut dyn Backend,
    query: &str,
    params: &HashMap<String, CypherValue>,
    format: OutputFormat,
) -> bool {
    // Handle transaction control
    let upper = query.to_ascii_uppercase();
    let upper = upper.trim();
    if upper == "BEGIN" {
        match backend.begin_tx() {
            Ok(()) => println!("Transaction started."),
            Err(e) => {
                display_error(&e);
                return false;
            }
        }
        return true;
    }
    if upper == "COMMIT" {
        match backend.commit_tx() {
            Ok(()) => println!("Transaction committed."),
            Err(e) => {
                display_error(&e);
                return false;
            }
        }
        return true;
    }
    if upper == "ROLLBACK" {
        match backend.rollback_tx() {
            Ok(()) => println!("Transaction rolled back."),
            Err(e) => {
                display_error(&e);
                return false;
            }
        }
        return true;
    }

    match backend.execute(query, params) {
        Ok(result) => {
            display_results(
                &result.columns,
                &result.rows,
                &result.stats,
                result.plan.as_ref(),
                format,
            );
            true
        }
        Err(e) => {
            display_error_with_position(query, &e);
            false
        }
    }
}

// -- Script file execution --------------------------------------------

fn run_script_file(
    backend: &mut dyn Backend,
    path: &str,
    params: &HashMap<String, CypherValue>,
    format: OutputFormat,
) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            display_error(&format!("Failed to read script '{}': {}", path, e));
            return false;
        }
    };
    execute_statements(backend, &content, params, format)
}

// -- Interactive REPL -------------------------------------------------

fn run_repl(backend: &mut dyn Backend, mut format: OutputFormat, highlighting: bool) {
    let schema_words = helper::new_schema_words();

    // Print connection banner
    print_banner(backend);

    // Populate schema words from the database
    {
        let words = backend.fetch_schema_words();
        if let Ok(mut sw) = schema_words.lock() {
            *sw = words;
        }
    }

    let helper = CypherHelper::new(highlighting, schema_words.clone());

    let config = rustyline::Config::builder()
        .auto_add_history(true)
        .completion_type(rustyline::CompletionType::List)
        .build();

    let mut rl = rustyline::Editor::with_config(config).expect("failed to create editor");
    rl.set_helper(Some(helper));

    // Load history
    let history_path = dirs_history_path();
    if let Some(ref path) = history_path {
        let _ = rl.load_history(path);
    }

    // Query parameters set via :param
    let mut params: HashMap<String, CypherValue> = HashMap::new();

    loop {
        let prompt = match backend.current_database() {
            Some(db) => format!("{}> ", db),
            None => "cypher> ".to_string(),
        };
        let readline = rl.readline(&prompt);

        match readline {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Meta-commands
                if trimmed.starts_with(':') {
                    match handle_meta_command(
                        trimmed,
                        &mut format,
                        &mut params,
                        backend,
                        &schema_words,
                    ) {
                        MetaResult::Continue => continue,
                        MetaResult::Quit => break,
                    }
                }

                // Execute as Cypher (strip trailing semicolons handled by split_statements)
                let stmts = split_statements(trimmed);
                for stmt in stmts {
                    let is_write = traverse_server::is_write_query(stmt);
                    let start = std::time::Instant::now();

                    // Handle transaction control
                    let upper = stmt.to_ascii_uppercase();
                    let upper_trimmed = upper.trim();
                    if upper_trimmed == "BEGIN" {
                        match backend.begin_tx() {
                            Ok(()) => println!("Transaction started."),
                            Err(e) => display_error(&e),
                        }
                        continue;
                    }
                    if upper_trimmed == "COMMIT" {
                        match backend.commit_tx() {
                            Ok(()) => println!("Transaction committed."),
                            Err(e) => display_error(&e),
                        }
                        continue;
                    }
                    if upper_trimmed == "ROLLBACK" {
                        match backend.rollback_tx() {
                            Ok(()) => println!("Transaction rolled back."),
                            Err(e) => display_error(&e),
                        }
                        continue;
                    }

                    match backend.execute(stmt, &params) {
                        Ok(result) => {
                            display_results(
                                &result.columns,
                                &result.rows,
                                &result.stats,
                                result.plan.as_ref(),
                                format,
                            );
                            let elapsed = start.elapsed();
                            let us = elapsed.as_micros();
                            if us < 1000 {
                                println!("Ready, took {us}us");
                            } else {
                                println!("Ready, took {:.3}ms", elapsed.as_secs_f64() * 1000.0);
                            }

                            // Refresh schema words after writes
                            if is_write {
                                let words = backend.fetch_schema_words();
                                if let Ok(mut sw) = schema_words.lock() {
                                    *sw = words;
                                }
                            }
                        }
                        Err(e) => {
                            display_error_with_position(stmt, &e);
                        }
                    }
                }
                println!();
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                break;
            }
            Err(e) => {
                display_error(&format!("readline error: {e}"));
                break;
            }
        }
    }

    // Save history
    if let Some(ref path) = history_path {
        let _ = rl.save_history(path);
    }

    println!("Bye!");
}

fn print_banner(backend: &mut dyn Backend) {
    let info = backend.connection_info();
    let (nodes, edges) = backend.fetch_db_stats();
    println!("Traverse CLI v{}", env!("CARGO_PKG_VERSION"));
    if let Some(db) = backend.current_database() {
        println!(
            "Connected to {}, database '{}' ({} nodes, {} edges)",
            info, db, nodes, edges
        );
    } else {
        println!("Connected to {} ({} nodes, {} edges)", info, nodes, edges);
    }

    println!("Type :help for commands, :quit to exit");
    println!();
}

enum MetaResult {
    Continue,
    Quit,
}

fn handle_meta_command(
    cmd: &str,
    format: &mut OutputFormat,
    params: &mut HashMap<String, CypherValue>,
    backend: &mut dyn Backend,
    schema_words: &helper::SchemaWords,
) -> MetaResult {
    let parts: Vec<&str> = cmd.splitn(2, char::is_whitespace).collect();
    let command = parts[0];
    let args = parts.get(1).map(|s| s.trim()).unwrap_or("");

    match command {
        ":quit" | ":exit" | ":q" => MetaResult::Quit,
        ":help" | ":h" => {
            println!("Commands:");
            println!("  :help              Show this help");
            println!("  :quit              Exit the shell");
            println!("  :format <fmt>      Set output format (table, json, csv)");
            println!("  :param key => val  Set a query parameter");
            println!("  :params            Show all parameters");
            println!("  :params clear      Clear all parameters");
            println!("  :source <file>     Run a .cypher script file");
            println!("  :use <database>    Switch to a different database");
            println!("  :clear             Clear the screen");
            println!();
            println!("Transactions (Bolt mode):");
            println!("  BEGIN;             Start a transaction");
            println!("  COMMIT;            Commit the transaction");
            println!("  ROLLBACK;          Rollback the transaction");
            println!();
            println!("User management (admin only):");
            println!("  CREATE USER <name> SET PASSWORD '<pass>' [SET ROLE ADMIN|EDITOR|READER]");
            println!("  ALTER USER <name> SET PASSWORD '<pass>' | SET ROLE <role>");
            println!("  DROP USER <name>");
            println!("  SHOW USERS");
            println!();
            println!("Database management (Bolt mode):");
            println!("  :use <database>         Switch to a different database");
            println!("  SHOW DATABASES          List all databases");
            println!("  CREATE DATABASE <name>  Create a new database");
            println!("  DROP DATABASE <name>    Drop a database");
            println!();
            println!("Data import:");
            println!("  LOAD CSV WITH HEADERS FROM 'file:///path.csv' AS row ...");
            println!();
            println!("Enter Cypher queries terminated with ;");
            MetaResult::Continue
        }
        ":format" => {
            if args.is_empty() {
                println!("Current format: {:?}", format);
                println!("Usage: :format <table|json|csv>");
            } else if let Some(f) = OutputFormat::parse(args) {
                *format = f;
                println!("Output format set to {:?}", f);
            } else {
                println!("Unknown format '{}'. Valid: table, json, csv", args);
            }
            MetaResult::Continue
        }
        ":param" => {
            if args.is_empty() {
                println!("Usage: :param name => value");
                println!("  Values: 42, 3.14, 'text', true, false, null");
            } else if let Some((key, val)) = args.split_once("=>") {
                let key = key.trim().to_string();
                let val = val.trim();
                match parse_param_value(val) {
                    Ok(cv) => {
                        println!("  ${} = {}", key, cv);
                        params.insert(key, cv);
                    }
                    Err(e) => println!("Invalid value '{}': {}", val, e),
                }
            } else {
                println!("Usage: :param name => value");
            }
            MetaResult::Continue
        }
        ":params" => {
            if args == "clear" {
                params.clear();
                println!("Parameters cleared.");
            } else if params.is_empty() {
                println!("No parameters set. Use :param name => value");
            } else {
                println!("Parameters:");
                for (k, v) in params.iter() {
                    println!("  ${} = {}", k, v);
                }
            }
            MetaResult::Continue
        }
        ":source" => {
            if args.is_empty() {
                println!("Usage: :source <path.cypher>");
            } else {
                run_script_file(backend, args, params, *format);
                // Refresh schema after running a script
                let words = backend.fetch_schema_words();
                if let Ok(mut sw) = schema_words.lock() {
                    *sw = words;
                }
            }
            MetaResult::Continue
        }
        ":use" => {
            if args.is_empty() {
                match backend.current_database() {
                    Some(db) => println!("Current database: {}", db),
                    None => println!("No database selected."),
                }
            } else {
                let target = args.to_string();
                // Validate by querying SHOW DATABASES
                let empty = HashMap::new();
                match backend.execute("SHOW DATABASES", &empty) {
                    Ok(result) => {
                        let exists = result.rows.iter().any(|row| {
                            row.first()
                                .and_then(|v| v.as_str())
                                .map(|s| s == target)
                                .unwrap_or(false)
                        });
                        if exists {
                            match backend.set_database(&target) {
                                Ok(()) => {
                                    println!("Switched to database '{}'.", target);
                                    // Refresh schema words for the new database
                                    let words = backend.fetch_schema_words();
                                    if let Ok(mut sw) = schema_words.lock() {
                                        *sw = words;
                                    }
                                }
                                Err(e) => println!("Error: {}", e),
                            }
                        } else {
                            println!(
                                "Database '{}' not found. Use SHOW DATABASES to list available databases.",
                                target
                            );
                        }
                    }
                    Err(e) => {
                        // SHOW DATABASES may not be supported (e.g. non-Traverse server);
                        // try switching anyway
                        match backend.set_database(&target) {
                            Ok(()) => println!("Switched to database '{}'.", target),
                            Err(e2) => println!("Error: {}", e2),
                        }
                        let _ = e; // suppress unused warning
                    }
                }
            }
            MetaResult::Continue
        }
        ":clear" => {
            print!("\x1b[2J\x1b[H");
            MetaResult::Continue
        }
        _ => {
            println!(
                "Unknown command '{}'. Type :help for available commands.",
                command
            );
            MetaResult::Continue
        }
    }
}

fn dirs_history_path() -> Option<std::path::PathBuf> {
    home_dir().map(|home| home.join(".traverse_history"))
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}
