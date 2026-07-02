//! The per-commit pipeline (Task 1.2): clone/pull → `cargo bench --profile
//! profiling` → parse criterion tree → charts → categorized storage →
//! regression check → (every 10th commit) trend digest.
//!
//! Split in two layers: subprocess helpers (`ensure_checkout`, `bench_target`,
//! …) and the pure post-bench stage ([`process_results`]) that integration
//! tests drive against fixture criterion trees without running git or cargo.

use crate::cards::{self, AlertCardParams, AlertRow, ImageRef, RenderedCard};
use crate::charts;
use crate::config::{RepoConfig, Settings};
use crate::criterion_results::{self, Row};
use crate::digest;
use crate::state::{State, Verdict};
use crate::storage::{CommitRecord, RepoStore};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Everything a `run` invocation produced. Serialized (via the CLI) as the
/// stdout JSON contract for the relaying agent.
#[derive(Debug)]
pub struct RunOutcome {
    pub commit_dir: PathBuf,
    pub failed_targets: Vec<String>,
    pub cards: Vec<RenderedCard>,
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

/// A `credential.helper` snippet that feeds `$GITHUB_TOKEN` from the process
/// environment to git for https remotes — the token never appears in argv, and
/// without the env var set git falls back to anonymous access (fine for public
/// repos). BB9 populates the env var from its own credential when needed (D7).
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
    if checkout.exists() {
        eprintln!("removing broken checkout {} (no .git) and re-cloning", checkout.display());
        std::fs::remove_dir_all(&checkout)?;
    }
    std::fs::create_dir_all(work_root)?;
    let mut cmd = Command::new("git");
    cmd.args(git_credential_args(&repo.clone_url));
    cmd.arg("clone").arg("--recursive").arg(&repo.clone_url).arg(&checkout);
    run_cmd(&mut cmd, &format!("git clone {}", repo.clone_url))?;
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
    git(checkout, &fetch_branch, "git fetch")?;
    if git(checkout, &["checkout", "--force", "--detach", sha], "git checkout").is_err() {
        let fetch_sha: Vec<&str> = [&cred_refs[..], &["fetch", "origin", sha]].concat();
        git(checkout, &fetch_sha, "git fetch <sha>")?;
        git(checkout, &["checkout", "--force", "--detach", sha], "git checkout")?;
    }
    let update_subs: Vec<&str> =
        [&cred_refs[..], &["submodule", "update", "--init", "--recursive"]].concat();
    git(checkout, &update_subs, "git submodule update")?;
    Ok(())
}

/// Fetches the tracked branch and checks out its remote HEAD (detached). Used
/// by the nightly flamegraph pipeline, which profiles "current main", not a
/// specific commit.
pub fn checkout_branch_head(checkout: &Path, repo: &RepoConfig) -> anyhow::Result<()> {
    let cred = git_credential_args(&repo.clone_url);
    let cred_refs: Vec<&str> = cred.iter().map(String::as_str).collect();
    let fetch: Vec<&str> = [&cred_refs[..], &["fetch", "origin", &repo.branch]].concat();
    git(checkout, &fetch, "git fetch")?;
    git(checkout, &["checkout", "--force", "--detach", "FETCH_HEAD"], "git checkout FETCH_HEAD")?;
    let update_subs: Vec<&str> =
        [&cred_refs[..], &["submodule", "update", "--init", "--recursive"]].concat();
    git(checkout, &update_subs, "git submodule update")?;
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
/// to stderr for the relaying agent's process logs; the data we parse is
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

/// Splits a full row key (`group/subject[/workload…]`, as stored in
/// `state.json`) back into its parts using the known group prefix. The
/// LONGEST matching group wins — group ids may themselves contain `/`, so a
/// shorter group must not shadow a longer one (`a` vs `a/b`).
fn split_row_key<'a>(
    row_key: &'a str,
    groups: &BTreeMap<String, Vec<&Row>>,
) -> Option<(&'a str, &'a str, &'a str)> {
    groups
        .keys()
        .filter(|group| row_key.starts_with(&format!("{group}/")))
        .max_by_key(|group| group.len())
        .map(|group| {
            let rest = &row_key[group.len() + 1..];
            let (subject, workload) = rest.split_once('/').unwrap_or((rest, ""));
            (&row_key[..group.len()], subject, workload)
        })
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
    let ratios = criterion_results::compute_ratios(&rows);

    // Idempotence guard: a retried run of the sha we just processed (e.g. the
    // relaying agent re-invoking after a downstream delivery failure) must not
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
    let mut record = CommitRecord::new(meta.sha.clone(), meta.date.clone(), meta.rustc.clone());
    record.add_ratios(&ratios);
    record.failed_targets = failed_targets.clone();
    let commit_dir = store.write_commit_record(&record)?;

    // 2. Charts. Comparison table (test item x implementation p95 + headline
    //    ratio column) and the revm=100% speed bars — the two views the design
    //    doc's 对比图 asks for. Charts are derived artifacts: a rendering
    //    failure is logged, not fatal — raw.json is already on disk and the
    //    state update below must still happen.
    let is_headline = |s: &str| digest::is_headline_subject(s, &repo.headline_spec);
    let table = charts::build_compare_table(&rows, &ratios, &repo.headline_spec, is_headline);
    if !table.rows.is_empty() {
        if let Err(e) = charts::render_compare_table(
            &commit_dir.join("compare_table.png"),
            &format!("{} vs revm_pinned @ {}", repo.name, record.short_sha()),
            &table,
        ) {
            eprintln!("compare table chart failed (continuing): {e:#}");
        }
    }

    let speed_items: Vec<charts::SpeedBarItem> = ratios
        .iter()
        .filter_map(|wl| {
            let mut bars: Vec<(String, f64)> = wl
                .rows
                .iter()
                .filter(|r| is_headline(&r.subject))
                .filter_map(|r| {
                    r.ratio_vs_revm_pinned.map(|ratio| (r.subject.clone(), 100.0 / ratio))
                })
                .collect();
            if bars.is_empty() {
                return None;
            }
            bars.sort_by(|a, b| a.0.cmp(&b.0));
            bars.insert(0, ("revm_pinned".to_string(), 100.0));
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
            &format!("{} relative speed @ {} (revm_pinned = 100%)", repo.name, record.short_sha()),
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
        wl_rows.sort_by_key(|r| (r.subject != "revm_pinned", r.subject.clone()));
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
    let mut regressed = Vec::new();
    let mut recovered = Vec::new();
    if !is_rerun {
        for wl in &ratios {
            for ratio_row in &wl.rows {
                let Some(ratio) = ratio_row.ratio_vs_revm_pinned else { continue };
                let key = criterion_results::row_key(&wl.group, &ratio_row.subject, &wl.workload);
                let verdict = state.check_and_record(
                    &key,
                    ratio,
                    settings.regression_threshold_pct,
                    settings.rolling_window,
                );
                if !digest::is_headline_subject(&ratio_row.subject, &repo.headline_spec) {
                    continue;
                }
                match verdict {
                    Verdict::NewRegression { median, current, .. } => {
                        regressed.push(AlertRow { row_key: key, median, current });
                    }
                    Verdict::Recovered { median, current } => {
                        recovered.push(AlertRow { row_key: key, median, current });
                    }
                    Verdict::FirstRun | Verdict::Ok | Verdict::StillRegressed => {}
                }
            }
        }
    }

    let mut cards: Vec<RenderedCard> = Vec::new();
    if !regressed.is_empty() || !recovered.is_empty() {
        let mut groups_map: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
        for row in &rows {
            groups_map.entry(row.group.clone()).or_default().push(row);
        }
        // Attach the comparison chart plus the distribution plots of the
        // affected rows (capped to keep the card readable).
        let mut images = Vec::new();
        for (file, alt) in [
            ("compare_table.png", "对比表（p95 µs + 倍率）"),
            ("compare_bars.png", "相对速度（revm=100%）"),
        ] {
            let path = commit_dir.join(file);
            if path.is_file() {
                images.push(ImageRef::new(path, alt));
            }
        }
        for alert_row in regressed.iter().chain(&recovered).take(3) {
            if let Some((group, _subject, workload)) =
                split_row_key(&alert_row.row_key, &groups_map)
            {
                let dist = commit_dir.join(dist_file_name(group, workload));
                if dist.is_file() && !images.iter().any(|i| i.path == dist) {
                    images.push(ImageRef::new(dist, format!("{} 分布", alert_row.row_key)));
                }
            }
        }
        cards.push(cards::render_alert_card(&AlertCardParams {
            repo_name: &repo.name,
            github: &repo.github,
            sha: &meta.sha,
            regressed,
            recovered,
            images,
            threshold_pct: settings.regression_threshold_pct,
            window: settings.rolling_window,
        })?);
    }

    // 4. Digest batching: every DIGEST_BATCH_SIZE commits, roll up a trend
    //    card. A failed digest build is reported on stderr but doesn't fail
    //    the run — the counter is left un-reset so the next commit retries.
    if !is_rerun && state.bump_digest_counter(settings.digest_batch_size) {
        let records: Vec<CommitRecord> = store
            .load_recent_commit_records(settings.digest_batch_size as usize)?
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        match digest::build_digest(
            store,
            &repo.github,
            &repo.name,
            &repo.headline_spec,
            settings.regression_threshold_pct,
            &records,
        ) {
            Ok(outcome) => {
                cards.push(outcome.card);
                state.reset_digest_counter();
            }
            Err(e) => eprintln!("digest build failed (will retry next commit): {e:#}"),
        }
    }

    state.last_seen_sha = Some(meta.sha.clone());
    state.save(&store.state_path())?;

    Ok(RunOutcome { commit_dir, failed_targets, cards })
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
        // Dev/regen mode: reuse whatever criterion tree the checkout already
        // has (e.g. re-render charts without re-benching).
        if !criterion_dir.is_dir() {
            anyhow::bail!(
                "--skip-bench: no existing criterion tree at {}",
                criterion_dir.display()
            );
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
    fn test_git_credential_args_only_for_https_with_token() {
        // No token in the test env → no credential args even for https.
        std::env::remove_var("GITHUB_TOKEN");
        assert!(git_credential_args("https://github.com/megaeth-labs/mega-evm.git").is_empty());
        assert!(git_credential_args("git@github.com:megaeth-labs/mega-evm.git").is_empty());
    }
}
