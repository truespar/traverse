mod cypher_import;
mod import;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "traverse-admin",
    about = "Traverse offline administration tools",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", "Copyright (c) 2026 The Intelligence Company AB (publ). MIT OR Apache-2.0."),
    before_help = concat!("traverse-admin ", env!("CARGO_PKG_VERSION")),
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Bulk-import CSV files into a new .tvdb database file.
    Import {
        /// Node CSV files in LABEL=path.csv format (repeatable).
        #[arg(long = "nodes", value_name = "LABEL=FILE")]
        nodes: Vec<String>,

        /// Edge CSV files in TYPE=path.csv format (repeatable).
        #[arg(long = "edges", value_name = "TYPE=FILE")]
        edges: Vec<String>,

        /// Output .tvdb file path.
        #[arg(long, short)]
        output: String,

        /// ID type for node cross-referencing.
        #[arg(long, default_value = "string")]
        id_type: String,

        /// CSV field delimiter.
        #[arg(long, default_value = ",")]
        delimiter: String,

        /// Skip rows that fail to parse instead of aborting.
        #[arg(long)]
        skip_bad_rows: bool,
    },

    /// Import a .cypher file (one statement per line) into a new .tvdb database file.
    CypherImport {
        /// Input .cypher file path.
        #[arg(long, short)]
        input: String,

        /// Output .tvdb file path.
        #[arg(long, short)]
        output: String,

        /// Skip statements that fail instead of aborting.
        #[arg(long)]
        skip_errors: bool,

        /// Create indexes before import (e.g. "User:id" creates index on :User(id)).
        /// If omitted, indexes are auto-created when the first MATCH is seen.
        #[arg(long = "index", value_name = "LABEL:PROP")]
        indexes: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Import {
            nodes,
            edges,
            output,
            id_type,
            delimiter,
            skip_bad_rows,
        } => {
            if let Err(e) =
                import::run_import(&nodes, &edges, &output, &id_type, &delimiter, skip_bad_rows)
            {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::CypherImport {
            input,
            output,
            skip_errors,
            indexes,
        } => {
            // Convert "Label:prop" to CREATE INDEX statements
            let pre_indexes: Vec<String> = indexes
                .iter()
                .filter_map(|s| {
                    let (label, prop) = s.split_once(':')?;
                    let name = format!("idx_{}_{}", label.to_lowercase(), prop.to_lowercase());
                    Some(format!("CREATE INDEX {name} FOR (n:{label}) ON (n.{prop})"))
                })
                .collect();
            if let Err(e) =
                cypher_import::run_cypher_import(&input, &output, skip_errors, &pre_indexes)
            {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }
}
