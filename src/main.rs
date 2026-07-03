//! CLI entry point. Each subcommand prints exactly one JSON document to
//! stdout — a summary of what the run produced and which factual events
//! occurred. All logs go to stderr.
//! This tool produces data only; composing/sending Lark cards is the
//! consuming agent's job (see `skill/`).

use clap::{Parser, Subcommand};
use mega_bench_reporter::config::Config;
use mega_bench_reporter::pipeline::Event;
use mega_bench_reporter::storage::RepoStore;
use mega_bench_reporter::{digest, flamegraph, pipeline};
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
    /// Nightly flame-graph archive (Linux: `perf`; macOS: `sample`).
    /// Writes SVGs under `flame/<day>/` only — no cards, nothing to relay.
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
    /// Manual trend chart over an arbitrary window of already-stored commits.
    /// Read-only: no bench, no state/events, independent of the automatic
    /// digest.
    Trend {
        /// Repo name — must match a `[[repos]]` entry in the config.
        #[arg(long)]
        repo: String,
        /// Path to the repos config file.
        #[arg(long, default_value = "repos.toml")]
        config: PathBuf,
        /// Root of the categorized data store (`<data-root>/<repo>/...`).
        #[arg(long)]
        data_root: PathBuf,
        /// Chart the most recent N stored commits (ignored when --from/--to
        /// is given).
        #[arg(long, default_value_t = 20)]
        last: usize,
        /// Oldest commit of the window (sha prefix, inclusive).
        #[arg(long)]
        from: Option<String>,
        /// Newest commit of the window (sha prefix, inclusive).
        #[arg(long)]
        to: Option<String>,
        /// Row key to chart, repeatable (exact or trailing `*`, e.g.
        /// `salt_dynamic_gas/*`); default = the configured headline rows.
        #[arg(long = "row")]
        rows: Vec<String>,
        /// Output directory (default
        /// `<data-root>/<repo>/trends/<day>-<first>..<last>`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// The stdout summary: one JSON document per invocation. The same facts are
/// durable on disk (`events.json` in the commit dir, `latest.json` at the
/// repo's data root), so losing this output loses nothing.
#[derive(Serialize)]
struct CliOutput {
    repo: String,
    sha: String,
    /// Directory the run's artifacts were written to (commit dir or flame dir).
    output_dir: PathBuf,
    /// Bench targets that failed this run (already marked in raw.json).
    /// Always present — a stable shape is easier on consumers.
    failed_targets: Vec<String>,
    /// Factual events from this run (regression / recovery / digest).
    events: Vec<Event>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Paths in the output JSON must survive the invoker's cwd
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
                events: outcome.events,
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
                // Archive-only: the flamegraph subcommand never emits events.
                events: Vec::new(),
            }
        }
        Command::Trend { repo, config, data_root, last, from, to, rows, out } => {
            let cfg = Config::load(&config)?;
            let repo_cfg = cfg.repo(&repo)?;
            let settings = cfg.settings(repo_cfg)?;
            let data_root = canonical_data_root(data_root)?;
            let store = RepoStore::new(&data_root, &repo);
            let records: Vec<_> =
                store.load_commit_records()?.into_iter().map(|(_, r)| r).collect();
            let window = digest::select_window(records, last, from.as_deref(), to.as_deref())?;
            let outcome = digest::build_adhoc_trend(
                &store,
                &repo,
                &repo_cfg.headline_label(),
                |s| repo_cfg.is_headline(s),
                &rows,
                settings.regression_threshold_pct,
                &window,
                out,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "repo": repo,
                    "output_dir": outcome.dir,
                    "commits": outcome.commits,
                    "rows": outcome.rows,
                }))?
            );
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
