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

    /// Source document for rotate (agent mode; needs --pages and --degrees).
    #[arg(long)]
    rotate: Option<PathBuf>,

    /// Rotation angle: 90, 180, or 270 (agent mode, with --rotate).
    #[arg(long)]
    degrees: Option<u32>,

    /// Source document for reorder (agent mode; needs --order).
    #[arg(long)]
    reorder: Option<PathBuf>,

    /// New page order: every page number exactly once, e.g. `3,1,2` (with --reorder).
    #[arg(long)]
    order: Option<String>,

    /// Source document for delete-pages (agent mode; needs --pages).
    #[arg(long)]
    delete_pages: Option<PathBuf>,

    /// Source document for compress (agent mode).
    #[arg(long)]
    compress: Option<PathBuf>,

    /// Source document for info (agent mode; read-only report).
    #[arg(long)]
    info: Option<PathBuf>,

    /// 1-based page ranges, e.g. `1-3,5` (agent mode, with --split / --rotate / --delete-pages).
    #[arg(long)]
    pages: Option<String>,

    /// Output path (agent mode; per-operation defaults live in the workspace).
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

/// Agent-mode result shapes: write operations report a file, `info` reports
/// document facts. Both serialize to a single stable JSON document.
enum AgentOutcome {
    Op(engine::OperationReport),
    Info(engine::InfoReport),
}

/// Agent mode: one operation, one JSON document on stdout, status as exit code.
fn run_agent(cli: &Cli, engine: &Engine) -> i32 {
    let outcome: (&str, Result<AgentOutcome, AppError>) = if !cli.merge.is_empty() {
        let output = default_output(cli, "merged.pdf");
        ("merge", engine.merge(&cli.merge, &output).map(AgentOutcome::Op))
    } else if let Some(source) = &cli.rotate {
        let output = default_output(cli, "rotated.pdf");
        match (&cli.pages, cli.degrees) {
            (Some(spec), Some(degrees)) => {
                ("rotate", engine.rotate(source, spec, degrees, &output).map(AgentOutcome::Op))
            }
            _ => (
                "rotate",
                Err(AppError::Operation(
                    "--rotate requires --pages <ranges> and --degrees 90|180|270".into(),
                )),
            ),
        }
    } else if let Some(source) = &cli.reorder {
        let output = default_output(cli, "reordered.pdf");
        match &cli.order {
            Some(order) => ("reorder", engine.reorder(source, order, &output).map(AgentOutcome::Op)),
            None => (
                "reorder",
                Err(AppError::Operation("--reorder requires --order <list, e.g. 3,1,2>".into())),
            ),
        }
    } else if let Some(source) = &cli.delete_pages {
        let output = default_output(cli, "trimmed.pdf");
        match &cli.pages {
            Some(spec) => {
                ("delete", engine.delete_pages(source, spec, &output).map(AgentOutcome::Op))
            }
            None => (
                "delete",
                Err(AppError::Operation("--delete-pages requires --pages <ranges>".into())),
            ),
        }
    } else if let Some(source) = &cli.compress {
        let output = default_output(cli, "compressed.pdf");
        ("compress", engine.compress(source, &output).map(AgentOutcome::Op))
    } else if let Some(source) = &cli.info {
        ("info", engine.info(source).map(AgentOutcome::Info))
    } else if let (Some(source), Some(spec)) = (&cli.split, &cli.pages) {
        let output = default_output(cli, "split.pdf");
        ("split", split_with_spec(engine, source, spec, &output).map(AgentOutcome::Op))
    } else if cli.split.is_some() {
        return agent_error("split", "--pages is required with --split");
    } else {
        return agent_error(
            "agent",
            "--json requires one of: --merge <files…> | --split <file> --pages <ranges> | \
             --rotate <file> --pages <ranges> --degrees 90|180|270 | --reorder <file> --order 3,1,2 \
             | --delete-pages <file> --pages <ranges> | --compress <file> | --info <file> \
             (all may take --output <path> and --workspace <dir>)",
        );
    };

    let (operation, result) = outcome;
    match result {
        Ok(AgentOutcome::Op(report)) => {
            println!("{}", to_json(&report));
            0
        }
        Ok(AgentOutcome::Info(report)) => {
            match serde_json::to_string(&report) {
                Ok(doc) => {
                    println!("{doc}");
                    0
                }
                Err(_) => agent_error("info", "json serialization failed"),
            }
        }
        Err(e) => agent_error(operation, &e.to_string()),
    }
}

fn default_output(cli: &Cli, fallback: &str) -> std::path::PathBuf {
    cli.output
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(fallback))
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

/// Stable field order: status first, then operation-specific fields.
fn to_json(report: &engine::OperationReport) -> String {
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
