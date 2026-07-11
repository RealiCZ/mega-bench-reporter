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
use crate::compare;
use crate::config::{RepoConfig, Settings};
use crate::criterion_results::{self, Row, WorkloadRatios};
use crate::digest;
use crate::git::{self, CommitMeta};
use crate::instructions::{self, InstrCollection, InstrWorkloadRatios};
use crate::lane::Lane;
use crate::state::{RowHistory, State, Thresholds, Verdict};
use crate::storage::{CommitRecord, RepoStore};
use crate::subprocess::run_streaming;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A factual thing that happened during a run — the consumer decides what (if
/// anything) to do about it. Persisted as `events.json` in the commit dir and
/// echoed in the CLI's stdout summary.
///
/// `metric` names the lane an alert came from ([`Lane::metric_field`]):
/// absent (`None`, skipped in the JSON) for the walltime lane — so pre-lane
/// consumers see identical events — and `"instructions"` for the
/// instruction-count lane.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A headline row rose past the regression threshold this run (fires once
    /// per regression — latched until recovery).
    Regression {
        row_key: String,
        baseline_median: f64,
        current: f64,
        pct_over: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric: Option<String>,
        /// The instructions lane's cross-check for this row (see
        /// [`InstrAnnotation`]). Present on **walltime** events only; omitted
        /// (not `null`) on instructions events, which annotate nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<InstrAnnotation>,
    },
    /// A previously-regressed headline row dropped back within the recovery
    /// threshold of its frozen pre-regression median.
    Recovery {
        row_key: String,
        baseline_median: f64,
        current: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric: Option<String>,
        /// The instructions lane's cross-check for this row — walltime events
        /// only (see [`Event::Regression::instructions`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<InstrAnnotation>,
    },
    /// A digest window completed; its data is in `dir` (repo-relative).
    Digest { dir: String },
}

/// What the instructions lane saw for a row when its **walltime** alert fired:
/// a stateless cross-check that corroborates (or contradicts) the walltime
/// signal. Attached to walltime regression/recovery events only.
///
/// It reads the instructions lane's *pre-update* rolling median for the same
/// row — the exact median the instructions lane's own check compares against
/// this run — and its regression threshold, symmetric in both directions; the
/// instructions latch and any hysteresis play no part.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstrAnnotation {
    /// `(current_instr_ratio / instr_rolling_median - 1) * 100`, rounded to two
    /// decimals. `null` when the row has no instructions data this run or no
    /// instructions rolling median yet — paired with `verdict: "missing"`.
    pub ratio_delta_pct: Option<f64>,
    pub verdict: InstrVerdict,
}

/// The instructions cross-check outcome. `up`/`down` mean the instructions
/// count moved at least the instructions regression threshold in that
/// direction; `flat` is within it; `missing` is no comparable data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstrVerdict {
    Flat,
    Up,
    Down,
    Missing,
}

/// The pre-update instructions view the walltime lane annotates its events
/// with: this run's instructions ratio per row (`current`), the instructions
/// lane's rolling windows *before* this run folds in (`instr_rows`), and the
/// instructions regression threshold. Borrowed, not owned — the walltime
/// [`check_lane`] runs before the instructions one, so `instr_rows` is still
/// the pre-update state at annotation time.
struct InstrAnnotationCtx<'a> {
    current: &'a BTreeMap<String, f64>,
    instr_rows: &'a BTreeMap<String, RowHistory>,
    regression_pct: f64,
}

impl InstrAnnotationCtx<'_> {
    /// The annotation for one row key. `missing` when either the current
    /// instructions ratio or a pre-update rolling median is absent.
    fn annotation_for(&self, row_key: &str) -> InstrAnnotation {
        let current = self.current.get(row_key).copied();
        let median = self
            .instr_rows
            .get(row_key)
            .and_then(|h| median_opt(&h.recent_ratios))
            .filter(|m| *m > 0.0);
        match (current, median) {
            (Some(current), Some(median)) => {
                let delta = (current / median - 1.0) * 100.0;
                let verdict = if delta >= self.regression_pct {
                    InstrVerdict::Up
                } else if delta <= -self.regression_pct {
                    InstrVerdict::Down
                } else {
                    InstrVerdict::Flat
                };
                // Two-decimal percent precision (the repo has no other percent
                // rounding convention to inherit).
                let ratio_delta_pct = (delta * 100.0).round() / 100.0;
                InstrAnnotation { ratio_delta_pct: Some(ratio_delta_pct), verdict }
            }
            _ => InstrAnnotation { ratio_delta_pct: None, verdict: InstrVerdict::Missing },
        }
    }
}

/// Median of a rolling window (empty = `None`), for the instructions
/// annotation's pre-update comparison.
fn median_opt(values: &std::collections::VecDeque<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("ratios are finite"));
    let n = sorted.len();
    Some(if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 })
}

/// The lane-agnostic view of one workload's ratio table — everything the
/// unified verdict loop ([`check_lane`]) reads from either lane's ratio type.
trait LaneWorkloadRatios {
    fn group(&self) -> &str;
    fn workload(&self) -> &str;
    /// Each subject and its ratio vs the baseline (`None` = no baseline row).
    fn subject_ratios(&self) -> impl Iterator<Item = (&str, Option<f64>)>;
}

impl LaneWorkloadRatios for WorkloadRatios {
    fn group(&self) -> &str {
        &self.group
    }
    fn workload(&self) -> &str {
        &self.workload
    }
    fn subject_ratios(&self) -> impl Iterator<Item = (&str, Option<f64>)> {
        self.rows.iter().map(|r| (r.subject.as_str(), r.ratio_vs_baseline))
    }
}

impl LaneWorkloadRatios for InstrWorkloadRatios {
    fn group(&self) -> &str {
        &self.group
    }
    fn workload(&self) -> &str {
        &self.workload
    }
    fn subject_ratios(&self) -> impl Iterator<Item = (&str, Option<f64>)> {
        self.rows.iter().map(|r| (r.subject.as_str(), r.ratio_vs_baseline))
    }
}

/// One lane's regression pass — the verdict protocol both lanes share. Every
/// row with a ratio is folded into `lane_rows` (that lane's rolling windows
/// and latches; history is cheap); only headline-family rows emit events,
/// tagged with the lane's `metric` marker.
#[allow(clippy::too_many_arguments)]
fn check_lane(
    lane: Lane,
    ratios: &[impl LaneWorkloadRatios],
    thresholds: Thresholds,
    lane_rows: &mut BTreeMap<String, RowHistory>,
    rolling_window: usize,
    is_headline: impl Fn(&str) -> bool,
    annotate: Option<&InstrAnnotationCtx>,
    events: &mut Vec<Event>,
) {
    for wl in ratios {
        for (subject, ratio) in wl.subject_ratios() {
            let Some(ratio) = ratio else { continue };
            let key = criterion_results::row_key(wl.group(), subject, wl.workload());
            let verdict =
                State::check_and_record_in(lane_rows, &key, ratio, thresholds, rolling_window);
            if !is_headline(subject) {
                continue;
            }
            let metric = lane.metric_field().map(str::to_string);
            // Only the walltime lane annotates; the instructions lane passes
            // `annotate: None`, so its events omit the field entirely.
            let instructions = annotate.map(|ctx| ctx.annotation_for(&key));
            match verdict {
                Verdict::NewRegression { median, current, pct_over } => {
                    events.push(Event::Regression {
                        row_key: key,
                        baseline_median: median,
                        current,
                        pct_over,
                        metric,
                        instructions,
                    });
                }
                Verdict::Recovered { median, current } => {
                    events.push(Event::Recovery {
                        row_key: key,
                        baseline_median: median,
                        current,
                        metric,
                        instructions,
                    });
                }
                Verdict::FirstRun | Verdict::Ok | Verdict::StillRegressed => {}
            }
        }
    }
}

/// Everything a `run` invocation produced. Serialized (via the CLI) as the
/// stdout JSON summary for the consuming agent.
#[derive(Debug)]
pub struct RunOutcome {
    pub commit_dir: PathBuf,
    pub failed_targets: Vec<String>,
    /// Instructions-lane per-target failures; `None` when the lane is off,
    /// skipped, or fully clean (mirrors `CommitRecord::instr_failed_targets`).
    pub instr_failed_targets: Option<Vec<String>>,
    pub events: Vec<Event>,
}

// ---------------------------------------------------------------------------
// Bench runner
// ---------------------------------------------------------------------------

/// Runs one bench target. With no configured `bench_profile` this is
/// `cargo bench -p <pkg> --bench <target> -- --output-format bencher` — the
/// invocation the scheduled walltime layer standardized on, so the numbers
/// stay comparable across runs (mega-evm's per-PR walltime flow has been
/// superseded by CodSpeed instruction-count CI). Output streams to stderr
/// for the invoker's process logs; the data we parse is criterion's
/// `target/criterion` tree, written as a side effect of any run.
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
    // run_streaming pipes the bencher lines to stderr instead of inheriting.
    cmd.arg("--").arg("--output-format").arg("bencher");
    run_streaming(cmd, &format!("cargo bench --bench {target}"))
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

/// Relative-speed bar items for one lane's ratio table (walltime →
/// `compare_bars.png`, instructions → `instr_bars.png`): per workload, the
/// baseline at 100% plus each headline subject's `100 / ratio` (baseline =
/// 100%, lower = more overhead). Workloads with no headline ratio are dropped.
/// Shared across lanes via [`LaneWorkloadRatios`] so both charts are built the
/// same way.
fn speed_bar_items(
    ratios: &[impl LaneWorkloadRatios],
    baseline_subject: &str,
    is_headline: impl Fn(&str) -> bool,
) -> Vec<charts::SpeedBarItem> {
    ratios
        .iter()
        .filter_map(|wl| {
            let mut bars: Vec<(String, f64)> = wl
                .subject_ratios()
                .filter(|(subject, _)| is_headline(subject))
                .filter_map(|(subject, ratio)| ratio.map(|r| (subject.to_string(), 100.0 / r)))
                .collect();
            if bars.is_empty() {
                return None;
            }
            bars.sort_by(|a, b| a.0.cmp(&b.0));
            bars.insert(0, (baseline_subject.to_string(), 100.0));
            let item = if wl.workload().is_empty() {
                wl.group().to_string()
            } else {
                format!("{}/{}", wl.group(), wl.workload())
            };
            Some(charts::SpeedBarItem { item, bars })
        })
        .collect()
}

/// The whole post-bench stage for one commit:
/// parse → record → charts → rolling-median regression check → digest batch.
/// `instr` is the instructions lane's collection — `None` when the lane is
/// off or was skipped, in which case every artifact is byte-identical to a
/// walltime-only run.
pub fn process_results(
    repo: &RepoConfig,
    settings: &Settings,
    store: &RepoStore,
    criterion_dir: &Path,
    meta: &CommitMeta,
    failed_targets: Vec<String>,
    instr: Option<InstrCollection>,
) -> anyhow::Result<RunOutcome> {
    let rows = criterion_results::scan(criterion_dir)?;
    let ratios = criterion_results::compute_ratios(&rows, &repo.baseline_subject);
    let instr_ratios =
        instr.as_ref().map(|c| instructions::compute_instr_ratios(&c.rows, &repo.baseline_subject));

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
    if let Some(instr_ratios) = &instr_ratios {
        record.add_instr_ratios(instr_ratios);
    }
    record.instr_failed_targets =
        instr.as_ref().map(|c| c.failed_targets.clone()).filter(|failed| !failed.is_empty());
    let commit_dir = store.write_commit_record(&record)?;

    // 2. Charts and structured tables. Derived artifacts: a rendering failure
    //    is logged, not fatal — raw.json is already on disk and the state
    //    update below must still happen.
    let is_headline = |s: &str| repo.is_headline(s);
    let subject_order = repo.subject_order();
    let table = compare::build_compare_table(
        &rows,
        &ratios,
        instr_ratios.as_deref(),
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

    // One run-wide subject→color mapping so every chart of this commit agrees.
    let subject_colors = charts::SubjectColors::new(
        &repo.baseline_subject,
        rows.iter().map(|r| r.subject.clone()),
        is_headline,
    );

    let speed_items = speed_bar_items(&ratios, &repo.baseline_subject, is_headline);
    if !speed_items.is_empty() {
        if let Err(e) = charts::render_speed_bars(
            &commit_dir.join("compare_bars.png"),
            &format!(
                "{} relative speed @ {} ({} = 100%)",
                repo.name,
                record.short_sha(),
                repo.baseline_subject
            ),
            &format!("relative speed, {} = 100% (lower = more overhead)", repo.baseline_subject),
            &speed_items,
            &subject_colors,
        ) {
            eprintln!("speed bars chart failed (continuing): {e:#}");
        }
    }

    // Instructions bars — the counts twin of compare_bars.png, rendered from
    // the same run-wide subject colors so the two charts agree. Written only
    // when the instructions lane produced headline rows for this commit.
    if let Some(instr_ratios) = instr_ratios.as_deref() {
        let instr_items = speed_bar_items(instr_ratios, &repo.baseline_subject, is_headline);
        if !instr_items.is_empty() {
            if let Err(e) = charts::render_speed_bars(
                &commit_dir.join("instr_bars.png"),
                &format!(
                    "{} relative instructions @ {} ({} = 100%)",
                    repo.name,
                    record.short_sha(),
                    repo.baseline_subject
                ),
                &format!(
                    "relative instruction count, {} = 100% (lower = more overhead)",
                    repo.baseline_subject
                ),
                &instr_items,
                &subject_colors,
            ) {
                eprintln!("instr bars chart failed (continuing): {e:#}");
            }
        }
    }

    // Violin per (group, workload) that has anything to compare (≥ 2 subjects).
    let mut by_workload: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
    for row in &rows {
        by_workload.entry((row.group.clone(), row.workload.clone())).or_default().push(row);
    }
    for ((group, workload), mut wl_rows) in by_workload {
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
            &subject_colors,
        ) {
            eprintln!("violin chart for {title} failed (continuing): {e:#}");
        }
    }

    // 3. Regression check against the rolling median. Every row with a ratio
    //    is recorded (history is cheap); only headline-family rows alert.
    //    Skipped entirely on a re-run of the same sha (see guard above).
    let mut events: Vec<Event> = Vec::new();
    if !is_rerun {
        // This run's instructions ratio per row_key: the walltime lane's
        // annotation reads it against the instructions lane's *pre-update*
        // rolling median. That median is still readable now because the
        // walltime pass below runs first — it never touches `instr_rows`.
        let mut instr_current: BTreeMap<String, f64> = BTreeMap::new();
        for wl in instr_ratios.as_deref().unwrap_or_default() {
            for row in &wl.rows {
                if let Some(ratio) = row.ratio_vs_baseline {
                    instr_current.insert(
                        criterion_results::row_key(&wl.group, &row.subject, &wl.workload),
                        ratio,
                    );
                }
            }
        }
        let annotate = InstrAnnotationCtx {
            current: &instr_current,
            instr_rows: &state.instr_rows,
            regression_pct: settings.thresholds(Lane::Instructions).regression_pct,
        };

        // Both lanes run the same protocol ([`check_lane`]) against their own
        // state map and threshold pair. Event order in events.json is stable:
        // all walltime events first, then the instructions lane's. Only the
        // walltime pass is annotated (`Some(&annotate)`); the instructions
        // pass passes `None`, so its events carry no `instructions` field.
        check_lane(
            Lane::Walltime,
            &ratios,
            settings.thresholds(Lane::Walltime),
            &mut state.rows,
            settings.rolling_window,
            is_headline,
            Some(&annotate),
            &mut events,
        );
        check_lane(
            Lane::Instructions,
            instr_ratios.as_deref().unwrap_or_default(),
            settings.thresholds(Lane::Instructions),
            &mut state.instr_rows,
            settings.rolling_window,
            is_headline,
            None,
            &mut events,
        );
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
            settings.instr_regression_threshold_pct,
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

    Ok(RunOutcome {
        commit_dir,
        failed_targets,
        instr_failed_targets: record.instr_failed_targets.clone(),
        events,
    })
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
                "--skip-bench only re-renders the last processed sha ({}); the existing \
                 criterion tree was not produced by {sha}",
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

    // Instructions lane: collected after the walltime benches on the same
    // checkout, only when configured. `collect` handles its own skipping
    // (non-Linux, tools missing) and never fails the run. Not collected in
    // --skip-bench regen mode — unlike the criterion tree, the callgrind
    // profiles are not kept around to re-parse.
    let instr = if skip_bench {
        None
    } else {
        repo.instructions.as_ref().and_then(|cfg| {
            instructions::collect(
                &checkout,
                repo,
                cfg,
                &work_root.join("_instr_profiles").join(&repo.name),
                std::env::consts::OS,
            )
        })
    };

    process_results(repo, settings, &store, &criterion_dir, &meta, failed_targets, instr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walltime_events_serialize_byte_identical_to_pre_lane_golden() {
        // Captured by serializing these exact events with the
        // pre-instructions-lane code: walltime events (metric: None) must not
        // change shape for existing consumers.
        let golden = r#"[
  {
    "type": "regression",
    "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
    "baseline_median": 2.0,
    "current": 2.3,
    "pct_over": 15.0
  },
  {
    "type": "recovery",
    "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
    "baseline_median": 2.0,
    "current": 2.02
  },
  {
    "type": "digest",
    "dir": "digests/20260702-abc1234..def5678"
  }
]"#;
        // With both the metric marker and the instructions annotation absent
        // (`None`), a walltime event serializes byte-identically to the
        // pre-lane shape — the additive fields are skipped, not emitted null.
        let events = vec![
            Event::Regression {
                row_key: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
                baseline_median: 2.0,
                current: 2.3,
                pct_over: 15.0,
                metric: None,
                instructions: None,
            },
            Event::Recovery {
                row_key: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
                baseline_median: 2.0,
                current: 2.02,
                metric: None,
                instructions: None,
            },
            Event::Digest { dir: "digests/20260702-abc1234..def5678".into() },
        ];
        assert_eq!(serde_json::to_string_pretty(&events).unwrap(), golden);
    }

    #[test]
    fn test_walltime_event_serializes_instructions_annotation() {
        // The pinned shape from skill/references/events.md: the annotation is
        // a `{ ratio_delta_pct, verdict }` object, ratio_delta_pct first.
        let event = Event::Regression {
            row_key: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
            baseline_median: 2.0,
            current: 2.3,
            pct_over: 15.0,
            metric: None,
            instructions: Some(InstrAnnotation {
                ratio_delta_pct: Some(-0.31),
                verdict: InstrVerdict::Flat,
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains(r#""instructions":{"ratio_delta_pct":-0.31,"verdict":"flat"}"#),
            "got {json}"
        );

        // A `missing` verdict keeps ratio_delta_pct present but null.
        let missing = Event::Recovery {
            row_key: "g/rex5/w".into(),
            baseline_median: 2.0,
            current: 2.0,
            metric: None,
            instructions: Some(InstrAnnotation {
                ratio_delta_pct: None,
                verdict: InstrVerdict::Missing,
            }),
        };
        let json = serde_json::to_string(&missing).unwrap();
        assert!(
            json.contains(r#""instructions":{"ratio_delta_pct":null,"verdict":"missing"}"#),
            "got {json}"
        );
    }

    #[test]
    fn test_instructions_events_carry_the_metric_marker_and_no_annotation() {
        let event = Event::Regression {
            row_key: "g/rex5/w".into(),
            baseline_median: 1.0,
            current: 1.05,
            pct_over: 5.0,
            metric: Lane::Instructions.metric_field().map(str::to_string),
            // Instructions events annotate nothing — the field is omitted.
            instructions: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["metric"], "instructions");
        assert!(
            value.get("instructions").is_none(),
            "instructions annotation must be omitted on instructions events: {value}"
        );
    }

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
