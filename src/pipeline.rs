//! The per-commit pipeline: clone/pull → `cargo bench` → parse criterion tree
//! → charts + structured JSON → categorized storage → regression check
//! (emitted as events, not cards) → (every Nth commit) trend digest.
//!
//! This tool produces DATA ONLY — raw metrics, charts, and factual events.
//! Composing and delivering Lark cards is entirely the consuming agent's job,
//! guided by `skill/`.
//!
//! Split in two layers: subprocess helpers (`ensure_checkout`, `bench_target`,
//! …) and the pure post-bench stage ([`process_results`]) that integration
//! tests drive against fixture criterion trees without running git or cargo.

use crate::charts;
use crate::config::{RepoConfig, Settings};
use crate::criterion_results::{self, Row};
use crate::digest;
use crate::state::{State, Verdict};
use crate::storage::{CommitRecord, RepoStore};
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

/// Commit-level metadata stored in `raw.json`.
#[derive(Debug, Clone)]
pub struct CommitMeta {
    /// Full commit sha.
    pub sha: String,
    /// Committer date, RFC3339.
    pub date: String,
    /// `rustc --version` output.
    pub rustc: String,
}

// ---------------------------------------------------------------------------
// Subprocess layer
// ---------------------------------------------------------------------------

fn run_cmd(cmd: &mut Command, what: &str) -> anyhow::Result<String> {
    let output =
        cmd.output().map_err(|e| anyhow::anyhow!("failed to spawn {what} ({cmd:?}): {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{what} failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git(checkout: &Path, args: &[&str], what: &str) -> anyhow::Result<String> {
    run_cmd(Command::new("git").arg("-C").arg(checkout).args(args), what)
}

/// Retries a git operation that talks to the network (clone / fetch /
/// submodule update) with backoff. Transient failures to reach the remote
/// (observed in the wild: SSL connection timeouts to GitHub) shouldn't fail
/// an entire bench run. `backoff_secs` has one entry per retry.
fn with_network_retry<T>(
    what: &str,
    backoff_secs: &[u64],
    mut op: impl FnMut() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut attempt = 0;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if attempt < backoff_secs.len() => {
                let wait = backoff_secs[attempt];
                attempt += 1;
                eprintln!(
                    "{what} failed (attempt {attempt}/{}, retrying in {wait}s): {e:#}",
                    backoff_secs.len() + 1
                );
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Default backoff for git network operations: two retries.
const GIT_RETRY_BACKOFF_SECS: &[u64] = &[5, 15];

/// A `credential.helper` snippet that feeds `$GITHUB_TOKEN` from the process
/// environment to git for https remotes — the token never appears in argv, and
/// without the env var set git falls back to anonymous access (fine for public
/// repos). The invoking agent populates the env var when needed.
const TOKEN_CREDENTIAL_HELPER: &str =
    "!f() { echo username=x-access-token; echo \"password=${GITHUB_TOKEN}\"; }; f";

fn git_credential_args(clone_url: &str) -> Vec<String> {
    if clone_url.starts_with("https://") && std::env::var("GITHUB_TOKEN").is_ok() {
        vec!["-c".into(), format!("credential.helper={TOKEN_CREDENTIAL_HELPER}")]
    } else {
        Vec::new()
    }
}

/// Clones (first run) or reuses the tracked repo's checkout under
/// `<work_root>/<repo name>`. Submodules included — mega-evm needs them.
/// A leftover directory without `.git` (an interrupted first clone) is
/// removed and re-cloned instead of wedging every subsequent run — the
/// checkout dir is fully machine-managed scratch.
pub fn ensure_checkout(work_root: &Path, repo: &RepoConfig) -> anyhow::Result<PathBuf> {
    let checkout = work_root.join(&repo.name);
    if checkout.join(".git").exists() {
        return Ok(checkout);
    }
    std::fs::create_dir_all(work_root)?;
    with_network_retry(&format!("git clone {}", repo.clone_url), GIT_RETRY_BACKOFF_SECS, || {
        // A leftover from an interrupted/failed previous attempt would make
        // `git clone` refuse with "destination exists" — clear it first.
        if checkout.exists() {
            eprintln!("removing broken checkout {} (no .git) and re-cloning", checkout.display());
            std::fs::remove_dir_all(&checkout)?;
        }
        let mut cmd = Command::new("git");
        cmd.args(git_credential_args(&repo.clone_url));
        cmd.arg("clone").arg("--recursive").arg(&repo.clone_url).arg(&checkout);
        run_cmd(&mut cmd, "git clone")?;
        Ok(())
    })?;
    Ok(checkout)
}

/// Fetches the tracked branch and checks out `sha` (detached), updating
/// submodules. `--force` so tracked files rewritten by a previous bench run
/// (classic: a regenerated `Cargo.lock`) can't wedge the checkout. Falls back
/// to fetching the sha directly if the branch fetch didn't make it reachable
/// (e.g. a force-pushed branch).
pub fn checkout_commit(checkout: &Path, repo: &RepoConfig, sha: &str) -> anyhow::Result<()> {
    let cred = git_credential_args(&repo.clone_url);
    let cred_refs: Vec<&str> = cred.iter().map(String::as_str).collect();

    let fetch_branch: Vec<&str> = [&cred_refs[..], &["fetch", "origin", &repo.branch]].concat();
    with_network_retry("git fetch", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &fetch_branch, "git fetch").map(|_| ())
    })?;
    if git(checkout, &["checkout", "--force", "--detach", sha], "git checkout").is_err() {
        let fetch_sha: Vec<&str> = [&cred_refs[..], &["fetch", "origin", sha]].concat();
        with_network_retry("git fetch <sha>", GIT_RETRY_BACKOFF_SECS, || {
            git(checkout, &fetch_sha, "git fetch <sha>").map(|_| ())
        })?;
        git(checkout, &["checkout", "--force", "--detach", sha], "git checkout")?;
    }
    let update_subs: Vec<&str> =
        [&cred_refs[..], &["submodule", "update", "--init", "--recursive"]].concat();
    with_network_retry("git submodule update", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &update_subs, "git submodule update").map(|_| ())
    })?;
    Ok(())
}

/// Fetches the tracked branch and checks out its remote HEAD (detached). Used
/// by the nightly flamegraph pipeline, which profiles "current main", not a
/// specific commit.
pub fn checkout_branch_head(checkout: &Path, repo: &RepoConfig) -> anyhow::Result<()> {
    let cred = git_credential_args(&repo.clone_url);
    let cred_refs: Vec<&str> = cred.iter().map(String::as_str).collect();
    let fetch: Vec<&str> = [&cred_refs[..], &["fetch", "origin", &repo.branch]].concat();
    with_network_retry("git fetch", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &fetch, "git fetch").map(|_| ())
    })?;
    git(checkout, &["checkout", "--force", "--detach", "FETCH_HEAD"], "git checkout FETCH_HEAD")?;
    let update_subs: Vec<&str> =
        [&cred_refs[..], &["submodule", "update", "--init", "--recursive"]].concat();
    with_network_retry("git submodule update", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &update_subs, "git submodule update").map(|_| ())
    })?;
    Ok(())
}

/// The checkout's current HEAD sha.
pub fn head_sha(checkout: &Path) -> anyhow::Result<String> {
    git(checkout, &["rev-parse", "HEAD"], "git rev-parse HEAD")
}

/// Commit metadata straight from the checkout: committer date + rustc version
/// (run inside the checkout so a `rust-toolchain.toml` is honored).
pub fn commit_meta(checkout: &Path, sha: &str) -> anyhow::Result<CommitMeta> {
    let date = git(checkout, &["show", "-s", "--format=%cI", sha], "git show")?;
    let rustc =
        run_cmd(Command::new("rustc").arg("--version").current_dir(checkout), "rustc --version")?;
    Ok(CommitMeta { sha: sha.to_string(), date, rustc })
}

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
        // Our own stdout carries exactly one JSON document per invocation, so
        // the bencher lines are streamed to stderr instead of inherited.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn cargo bench --bench {target}: {e}"))?;
    let mut child_stdout = child.stdout.take().expect("stdout piped");
    let copied = std::io::copy(&mut child_stdout, &mut std::io::stderr());
    if copied.is_err() {
        // Don't leave a running bench behind if the stdout drain broke.
        let _ = child.kill();
        let _ = child.wait();
        copied?;
    }
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
    let checkout = ensure_checkout(work_root, repo)?;
    checkout_commit(&checkout, repo, sha)?;
    let meta = commit_meta(&checkout, sha)?;

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

    #[test]
    fn test_with_network_retry_retries_then_succeeds() {
        let mut attempts = 0;
        let result = with_network_retry("op", &[0, 0], || {
            attempts += 1;
            if attempts < 3 {
                anyhow::bail!("transient");
            }
            Ok(attempts)
        })
        .unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn test_with_network_retry_gives_up_after_backoff_exhausted() {
        let mut attempts = 0;
        let result: anyhow::Result<()> = with_network_retry("op", &[0], || {
            attempts += 1;
            anyhow::bail!("still down")
        });
        assert!(result.is_err());
        assert_eq!(attempts, 2, "one initial try + one retry");
    }

    #[test]
    fn test_git_credential_args_only_for_https_with_token() {
        // No token in the test env → no credential args even for https.
        std::env::remove_var("GITHUB_TOKEN");
        assert!(git_credential_args("https://github.com/megaeth-labs/mega-evm.git").is_empty());
        assert!(git_credential_args("git@github.com:megaeth-labs/mega-evm.git").is_empty());
    }
}
