//! CLI entry point (Task 1.1). Subcommands take `(repo, sha)` or a schedule
//! tag and print exactly one JSON document to stdout — the ready-to-post
//! card(s) plus attachment paths. All logs go to stderr. Any agent that can
//! "run a command, read JSON" can drive this; nothing here is BB9-specific.

use clap::{Parser, Subcommand};
use mega_bench_reporter::cards::RenderedCard;
use mega_bench_reporter::config::Config;
use mega_bench_reporter::{flamegraph, pipeline};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mega-bench-reporter", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Per-commit pipeline: checkout, bench, chart, store, regression-check,
    /// and (every 10th commit) trend digest.
    Run {
        /// Repo name — must match a `[[repos]]` entry in the config.
        #[arg(long)]
        repo: String,
        /// Full commit sha to bench.
        #[arg(long)]
        sha: String,
        /// Reuse the checkout's existing criterion tree instead of benching
        /// (dev/regen mode, e.g. re-rendering charts).
        #[arg(long)]
        skip_bench: bool,
        /// Path to the repos config file.
        #[arg(long, default_value = "repos.toml")]
        config: PathBuf,
        /// Root of the categorized data store (`<data-root>/<repo>/...`).
        #[arg(long)]
        data_root: PathBuf,
        /// Where tracked repos get cloned; defaults to `<data-root>/_checkouts`.
        #[arg(long)]
        work_root: Option<PathBuf>,
    },
    /// Nightly flame-graph pipeline (Linux: `perf`; macOS: `sample`).
    Flamegraph {
        /// Repo name — must match a `[[repos]]` entry in the config.
        #[arg(long)]
        repo: String,
        /// Path to the repos config file.
        #[arg(long, default_value = "repos.toml")]
        config: PathBuf,
        /// Root of the categorized data store (`<data-root>/<repo>/...`).
        #[arg(long)]
        data_root: PathBuf,
        /// Where tracked repos get cloned; defaults to `<data-root>/_checkouts`.
        #[arg(long)]
        work_root: Option<PathBuf>,
    },
}

/// The stdout contract: one JSON document per invocation. `cards` is empty
/// when there is nothing to post (no regression, not a digest commit).
#[derive(Serialize)]
struct CliOutput {
    repo: String,
    sha: String,
    /// Directory the run's artifacts were written to (commit dir or flame dir).
    output_dir: PathBuf,
    /// Bench targets that failed this run (already marked in raw.json).
    /// Always present — a stable shape is easier on the relaying agent.
    failed_targets: Vec<String>,
    cards: Vec<RenderedCard>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Attachment paths in the output JSON must survive the relaying agent's cwd
/// being different from ours — canonicalize the data root up front.
fn canonical_data_root(data_root: PathBuf) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(&data_root)?;
    Ok(data_root.canonicalize()?)
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let output = match cli.command {
        Command::Run { repo, sha, skip_bench, config, data_root, work_root } => {
            let cfg = Config::load(&config)?;
            let repo_cfg = cfg.repo(&repo)?;
            let settings = cfg.settings(repo_cfg)?;
            let data_root = canonical_data_root(data_root)?;
            let work_root = work_root.unwrap_or_else(|| data_root.join("_checkouts"));
            let outcome = pipeline::run_commit_pipeline(
                repo_cfg, &settings, &sha, &data_root, &work_root, skip_bench,
            )?;
            CliOutput {
                repo,
                sha,
                output_dir: outcome.commit_dir,
                failed_targets: outcome.failed_targets,
                cards: outcome.cards,
            }
        }
        Command::Flamegraph { repo, config, data_root, work_root } => {
            let cfg = Config::load(&config)?;
            let repo_cfg = cfg.repo(&repo)?;
            let data_root = canonical_data_root(data_root)?;
            let work_root = work_root.unwrap_or_else(|| data_root.join("_checkouts"));
            let outcome = flamegraph::run_flamegraph_pipeline(repo_cfg, &data_root, &work_root)?;
            for day in &outcome.pruned {
                eprintln!("pruned flame/{day} (past retention)");
            }
            CliOutput {
                repo,
                sha: outcome.sha,
                output_dir: outcome.flame_dir,
                failed_targets: Vec::new(),
                cards: vec![outcome.card],
            }
        }
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
