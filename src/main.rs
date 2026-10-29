//! Central orchestrator for LocalPDF-TUI.
//!
//! Two execution modes share one codebase:
//!
//! * **Interactive** (default): launches the polished TUI dashboard.
//! * **Headless agent** (`--json`): bypasses the TUI entirely, runs one
//!   operation against the sandboxed engine, and prints exactly one
//!   deterministic JSON document to stdout. The exit code carries status.

mod engine;
mod error;
mod tui;
mod ui;

use clap::Parser;
use std::path::{Path, PathBuf};
use std::process;

use engine::Engine;
use error::AppError;

#[derive(Parser)]
#[command(
    name = "localpdf-tui",
    version,
    about = "LocalPDF-TUI — a private, terminal-native PDF toolkit (local ILovePDF)"
)]
struct Cli {
    /// Headless agent mode: emit one deterministic JSON document on stdout.
    #[arg(long)]
    json: bool,

    /// Files to merge in order (agent mode).
    #[arg(long, num_args = 1..)]
    merge: Vec<PathBuf>,

    /// Source document for split (agent mode).
    #[arg(long)]
    split: Option<PathBuf>,

    /// 1-based page ranges, e.g. `1-3,5` (agent mode, requires --split).
    #[arg(long)]
    pages: Option<String>,

    /// Output path (agent mode; defaults to merged.pdf / split.pdf in the workspace).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Sandbox root: all file access is pinned under this directory (default: CWD).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    let workspace = cli
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let engine = match Engine::new(&workspace) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(2);
        }
    };

    if cli.json {
        process::exit(run_agent(&cli, &engine));
    }

    match tui::run(tui::App::new(engine)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    }
}

/// Agent mode: one operation, one JSON document on stdout, status as exit code.
fn run_agent(cli: &Cli, engine: &Engine) -> i32 {
    let outcome: (&str, Result<engine::OperationReport, AppError>) = if !cli.merge.is_empty() {
        let output = cli.output.clone().unwrap_or_else(|| PathBuf::from("merged.pdf"));
        ("merge", engine.merge(&cli.merge, &output))
    } else if let (Some(source), Some(spec)) = (&cli.split, &cli.pages) {
        let output = cli.output.clone().unwrap_or_else(|| PathBuf::from("split.pdf"));
        ("split", split_with_spec(engine, source, spec, &output))
    } else if cli.split.is_some() {
        return agent_error("split", "--pages is required with --split");
    } else {
        return agent_error(
            "agent",
            "--json requires `--merge <files…> [--output out.pdf]` or `--split <file> --pages <ranges> [--output out.pdf]`",
        );
    };

    let (operation, result) = outcome;
    match result {
        Ok(report) => {
            println!("{}", to_json_success(&report));
            0
        }
        Err(e) => agent_error(operation, &e.to_string()),
    }
}

fn split_with_spec(
    engine: &Engine,
    source: &Path,
    spec: &str,
    output: &Path,
) -> Result<engine::OperationReport, AppError> {
    let ranges = engine::parse_ranges(spec)?;
    engine.split(source, &ranges, output)
}

/// Stable field order: status, operation, output_file, pages.
fn to_json_success(report: &engine::OperationReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| {
        r#"{"status":"error","operation":"unknown","error":"json serialization failed"}"#.to_string()
    })
}

/// The only failure shape in agent mode.
fn agent_error(operation: &str, message: &str) -> i32 {
    let doc = serde_json::json!({
        "status": "error",
        "operation": operation,
        "error": message,
    });
    println!("{}", doc.to_string());
    1
}
