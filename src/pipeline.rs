//! The per-commit pipeline: clone/pull → `cargo bench` → parse criterion tree
//! → charts + structured JSON → categorized storage → regression check
//! (emitted as events, not cards) → (every Nth commit) trend digest.
//!
//! This tool produces DATA ONLY — raw metrics, charts, and factual events.
//! Composing and delivering Lark cards is entirely the consuming agent's job,
//! guided by `skill/`.
//!
//! Split in two layers: the bench runner (`bench_target`, with the git side
//! in [`crate::git`]) and the pure post-bench stage ([`process_results`]) that
//! integration tests drive against fixture criterion trees without running
//! git or cargo.

use crate::charts;
use crate::config::{RepoConfig, Settings};
use crate::criterion_results::{self, Row};
use crate::digest;
use crate::git::{self, CommitMeta};
use crate::state::{State, Verdict};
use crate::storage::{CommitRecord, RepoStore};
use crate::subprocess::drain_stdout_to_stderr;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A factual thing that happened during a run — the consumer decides what (if
/// anything) to do about it. Persisted as `events.json` in the commit dir and
/// echoed in the CLI's stdout summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A headline row rose past the regression threshold this run (fires once
    /// per regression — latched until recovery).
    Regression { row_key: String, baseline_median: f64, current: f64, pct_over: f64 },
    /// A previously-regressed headline row dropped back under the threshold.
    Recovery { row_key: String, baseline_median: f64, current: f64 },
    /// A digest window completed; its data is in `dir` (repo-relative).
    Digest { dir: String },
}

/// Everything a `run` invocation produced. Serialized (via the CLI) as the
/// stdout JSON summary for the consuming agent.
#[derive(Debug)]
pub struct RunOutcome {
    pub commit_dir: PathBuf,
    pub failed_targets: Vec<String>,
    pub events: Vec<Event>,
}

// ---------------------------------------------------------------------------
// Bench runner
// ---------------------------------------------------------------------------

/// Runs one bench target. With no configured `bench_profile` this is exactly
/// the invocation the tracked repo's CI uses (mega-evm's benchmark.yml):
/// `cargo bench -p <pkg> --bench <target> -- --output-format bencher` — so the
/// numbers stay comparable with the per-PR `/benchmark` flow. Output streams
/// to stderr for the invoker's process logs; the data we parse is
/// criterion's `target/criterion` tree, written as a side effect of any run.
pub fn bench_target(
    checkout: &Path,
    repo: &RepoConfig,
    target: &str,
    bench_profile: Option<&str>,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(checkout).args(["bench", "-p", repo.package(), "--bench", target]);
    if let Some(profile) = bench_profile {
        cmd.args(["--profile", profile]);
    }
    let mut child = cmd
        .arg("--")
        .arg("--output-format")
        .arg("bencher")
        // The bencher lines are streamed to stderr instead of inherited.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn cargo bench --bench {target}: {e}"))?;
    drain_stdout_to_stderr(&mut child)?;
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("cargo bench --bench {target} failed ({status})");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Post-bench stage (no subprocesses — integration-testable with fixtures)
// ---------------------------------------------------------------------------

/// `dist_<group>[_<workload>].png`, criterion path separators flattened.
pub fn dist_file_name(group: &str, workload: &str) -> String {
    let sanitize = |s: &str| s.replace('/', "_");
    if workload.is_empty() {
        format!("dist_{}.png", sanitize(group))
    } else {
        format!("dist_{}_{}.png", sanitize(group), sanitize(workload))
    }
}

/// The whole post-bench stage for one commit:
/// parse → record → charts → rolling-median regression check → digest batch.
pub fn process_results(
    repo: &RepoConfig,
    settings: &Settings,
    store: &RepoStore,
    criterion_dir: &Path,
    meta: &CommitMeta,
    failed_targets: Vec<String>,
) -> anyhow::Result<RunOutcome> {
    let rows = criterion_results::scan(criterion_dir)?;
    let ratios = criterion_results::compute_ratios(&rows, &repo.baseline_subject);

    // Idempotence guard: a retried run of the sha we just processed (e.g. the
    // invoker re-running after losing the output) must not
    // fold the same ratios into the rolling window twice or double-bump the
    // digest counter. The record and charts are still refreshed.
    let mut state = State::load(&store.state_path())?;
    let is_rerun = state.last_seen_sha.as_deref() == Some(meta.sha.as_str());
    if is_rerun {
        eprintln!(
            "sha {} was already processed last run — refreshing artifacts without touching \
             regression state",
            meta.sha
        );
    }

    // 1. raw.json — the structured record is written before anything that can
    //    fail cosmetically (charts), so the data survives a rendering bug.
    let mut record = CommitRecord::new(
        meta.sha.clone(),
        meta.date.clone(),
        meta.rustc.clone(),
        repo.baseline_subject.clone(),
    );
    record.add_ratios(&ratios);
    record.failed_targets = failed_targets.clone();
    let commit_dir = store.write_commit_record(&record)?;

    // 2. Charts and structured tables. Derived artifacts: a rendering failure
    //    is logged, not fatal — raw.json is already on disk and the state
    //    update below must still happen.
    let is_headline = |s: &str| repo.is_headline(s);
    let subject_order = repo.subject_order();
    let table = charts::build_compare_table(
        &rows,
        &ratios,
        &repo.headline_label(),
        &repo.baseline_subject,
        &subject_order,
        is_headline,
    );
    if !table.rows.is_empty() {
        // Emitted as structured JSON — consumers build their own native
        // table from it instead of embedding a rendered image.
        if let Err(e) = serde_json::to_string_pretty(&table)
            .map_err(anyhow::Error::from)
            .and_then(|json| Ok(std::fs::write(commit_dir.join("compare_table.json"), json)?))
        {
            eprintln!("compare table json failed (continuing): {e:#}");
        }
    }

    let speed_items: Vec<charts::SpeedBarItem> = ratios
        .iter()
        .filter_map(|wl| {
            let mut bars: Vec<(String, f64)> = wl
                .rows
                .iter()
                .filter(|r| is_headline(&r.subject))
                .filter_map(|r| r.ratio_vs_baseline.map(|ratio| (r.subject.clone(), 100.0 / ratio)))
                .collect();
            if bars.is_empty() {
                return None;
            }
            bars.sort_by(|a, b| a.0.cmp(&b.0));
            bars.insert(0, (repo.baseline_subject.clone(), 100.0));
            let item = if wl.workload.is_empty() {
                wl.group.clone()
            } else {
                format!("{}/{}", wl.group, wl.workload)
            };
            Some(charts::SpeedBarItem { item, bars })
        })
        .collect();
    if !speed_items.is_empty() {
        if let Err(e) = charts::render_speed_bars(
            &commit_dir.join("compare_bars.png"),
            &format!(
                "{} relative speed @ {} ({} = 100%)",
                repo.name,
                record.short_sha(),
                repo.baseline_subject
            ),
            &repo.baseline_subject,
            &speed_items,
        ) {
            eprintln!("speed bars chart failed (continuing): {e:#}");
        }
    }

    // Violin per (group, workload) that has anything to compare (≥ 2 subjects).
    let mut by_workload: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
    for row in &rows {
        by_workload.entry((row.group.clone(), row.workload.clone())).or_default().push(row);
    }
    for ((group, workload), mut wl_rows) in by_workload.clone() {
        if wl_rows.len() < 2 {
            continue;
        }
        // Baseline first (stable color), then the rest alphabetically.
        wl_rows.sort_by_key(|r| (r.subject != repo.baseline_subject, r.subject.clone()));
        let title = if workload.is_empty() { group.clone() } else { format!("{group}/{workload}") };
        if let Err(e) = charts::render_violin(
            &commit_dir.join(dist_file_name(&group, &workload)),
            &format!("{title} — per-call distribution"),
            &wl_rows,
        ) {
            eprintln!("violin chart for {title} failed (continuing): {e:#}");
        }
    }

    // 3. Regression check against the rolling median. Every row with a ratio
    //    is recorded (history is cheap); only headline-family rows alert.
    //    Skipped entirely on a re-run of the same sha (see guard above).
    let mut events: Vec<Event> = Vec::new();
    if !is_rerun {
        for wl in &ratios {
            for ratio_row in &wl.rows {
                let Some(ratio) = ratio_row.ratio_vs_baseline else { continue };
                let key = criterion_results::row_key(&wl.group, &ratio_row.subject, &wl.workload);
                let verdict = state.check_and_record(
                    &key,
                    ratio,
                    settings.regression_threshold_pct,
                    settings.rolling_window,
                );
                if !is_headline(&ratio_row.subject) {
                    continue;
                }
                match verdict {
                    Verdict::NewRegression { median, current, pct_over } => {
                        events.push(Event::Regression {
                            row_key: key,
                            baseline_median: median,
                            current,
                            pct_over,
                        });
                    }
                    Verdict::Recovered { median, current } => {
                        events.push(Event::Recovery {
                            row_key: key,
                            baseline_median: median,
                            current,
                        });
                    }
                    Verdict::FirstRun | Verdict::Ok | Verdict::StillRegressed => {}
                }
            }
        }
    }

    // 4. Digest batching: every digest_batch_size commits, roll up the window
    //    into digests/ and emit a digest event. A failed digest build is
    //    reported on stderr but doesn't fail the run — the counter is left
    //    un-reset so the next commit retries.
    if !is_rerun && state.bump_digest_counter(settings.digest_batch_size) {
        let records: Vec<CommitRecord> = store
            .load_recent_commit_records(settings.digest_batch_size as usize)?
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        match digest::build_digest(
            store,
            &repo.name,
            &repo.headline_label(),
            is_headline,
            settings.regression_threshold_pct,
            &records,
        ) {
            Ok(outcome) => {
                let dir = outcome
                    .dir
                    .strip_prefix(store.root())
                    .unwrap_or(&outcome.dir)
                    .to_string_lossy()
                    .into_owned();
                events.push(Event::Digest { dir });
                state.reset_digest_counter();
            }
            Err(e) => eprintln!("digest build failed (will retry next commit): {e:#}"),
        }
    }

    // Durable copy of this run's events, written BEFORE the state is saved:
    // the consumer can always recover the facts from disk even if it lost the
    // stdout. A retry of the same sha deliberately does NOT overwrite this
    // file (its event list would be empty). A crash between this write and
    // the state save leaves last_seen_sha untouched, so the retry redoes the
    // run and rewrites this file consistently.
    if !is_rerun {
        std::fs::write(commit_dir.join("events.json"), serde_json::to_string_pretty(&events)?)?;
    }

    state.last_seen_sha = Some(meta.sha.clone());
    state.save(&store.state_path())?;
    // Discovery pointer: always points at the newest completed run.
    store.write_latest(&meta.sha, &commit_dir)?;

    Ok(RunOutcome { commit_dir, failed_targets, events })
}

// ---------------------------------------------------------------------------
// Full per-commit run
// ---------------------------------------------------------------------------

/// The complete `run` subcommand: checkout at `sha`, bench every configured
/// target (a failing target is marked in `raw.json` and skipped, not fatal —
/// unless *every* target failed, which means no data at all), then hand the
/// criterion tree to [`process_results`].
pub fn run_commit_pipeline(
    repo: &RepoConfig,
    settings: &Settings,
    sha: &str,
    data_root: &Path,
    work_root: &Path,
    skip_bench: bool,
) -> anyhow::Result<RunOutcome> {
    let store = RepoStore::new(data_root, &repo.name);
    // Held for the whole run: concurrent invocations share the checkout, the
    // criterion tree, and state.json.
    let _lock = store.acquire_lock()?;
    let checkout = git::ensure_checkout(work_root, repo)?;
    git::checkout_commit(&checkout, repo, sha)?;
    let meta = git::commit_meta(&checkout, sha)?;

    let criterion_dir = criterion_results::criterion_dir_for(&checkout);
    let mut failed_targets = Vec::new();
    if skip_bench {
        // Dev/regen mode: reuse the checkout's criterion tree to re-render
        // charts/records. The tree's provenance is only known for the last
        // processed sha — anything else would silently record another
        // commit's numbers under this sha and pollute the rolling medians.
        if !criterion_dir.is_dir() {
            anyhow::bail!(
                "--skip-bench: no existing criterion tree at {}",
                criterion_dir.display()
            );
        }
        let state = State::load(&store.state_path())?;
        if state.last_seen_sha.as_deref() != Some(sha) {
            anyhow::bail!(
                "--skip-bench only re-renders the last processed sha ({}); the existing                  criterion tree was not produced by {sha}",
                state.last_seen_sha.as_deref().unwrap_or("<none>")
            );
        }
        // Preserve the original run's failed-target markers instead of
        // silently erasing them from the regenerated record.
        let record = CommitRecord::new(
            sha.to_string(),
            meta.date.clone(),
            meta.rustc.clone(),
            repo.baseline_subject.clone(),
        );
        if let Ok(dir) = store.commit_dir(&record) {
            if let Ok(text) = std::fs::read_to_string(dir.join("raw.json")) {
                if let Ok(existing) = serde_json::from_str::<CommitRecord>(&text) {
                    failed_targets = existing.failed_targets;
                }
            }
        }
    } else {
        // Stale results from a previous commit's run must not leak into this one.
        if criterion_dir.exists() {
            std::fs::remove_dir_all(&criterion_dir)?;
        }
        for target in &repo.bench_targets {
            if let Err(e) = bench_target(&checkout, repo, target, settings.bench_profile.as_deref())
            {
                eprintln!("bench target '{target}' failed: {e:#}");
                failed_targets.push(target.clone());
            }
        }
        if failed_targets.len() == repo.bench_targets.len() {
            anyhow::bail!(
                "all {} bench targets failed — no data to report",
                repo.bench_targets.len()
            );
        }
    }

    process_results(repo, settings, &store, &criterion_dir, &meta, failed_targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dist_file_name_sanitizes_criterion_separators() {
        assert_eq!(
            dist_file_name("salt_dynamic_gas", "sstore_100"),
            "dist_salt_dynamic_gas_sstore_100.png"
        );
        assert_eq!(
            dist_file_name("salt_dynamic_gas", "sstore_100/x8"),
            "dist_salt_dynamic_gas_sstore_100_x8.png"
        );
        assert_eq!(dist_file_name("empty_transaction", ""), "dist_empty_transaction.png");
    }
}
