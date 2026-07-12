//! The "instructions" metric lane: deterministic CPU instruction counts (`Ir`)
//! per benchmark, collected with the CodSpeed runner's offline simulation mode
//! (`codspeed run --skip-upload --mode simulation -- cargo codspeed run`) and
//! parsed from the callgrind-text profiles it writes.
//!
//! Strictly additive beside the walltime lane: when the lane is off (no
//! `[repos.instructions]` config) or skipped (non-Linux host, tools missing),
//! nothing here runs and the walltime output is byte-identical to before.
//! A lane failure never fails the run — per-target failures are collected in
//! `instr_failed_targets` and the walltime data is unaffected — unless the
//! repo opts in via `require_instructions`, which turns a skip or failure
//! into a nonzero exit AFTER the walltime write sequence completes.

use crate::config::{InstructionsConfig, RepoConfig};
use crate::subprocess::run_streaming;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// One benchmark's instruction count, keyed exactly like the walltime lane's
/// `criterion_results::Row` (same `(group, subject, workload)` triple, same
/// `row_key` string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrRow {
    pub group: String,
    pub subject: String,
    pub workload: String,
    /// Total `Ir` (instructions retired) for the traced benchmark part.
    pub count: u64,
}

/// Everything one lane run produced: parsed rows plus the bench targets whose
/// build/run/parse failed (marked, not fatal — mirrors `failed_targets`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstrCollection {
    pub rows: Vec<InstrRow>,
    pub failed_targets: Vec<String>,
}

/// What [`collect`] came back with: a collection (possibly with per-target
/// failures inside), or a whole-lane skip carrying the human-readable reason
/// — the same text as the stderr note. The reason is data, not just a log
/// line, because `require_instructions = true` turns it into the run's error
/// message after the write sequence completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectOutcome {
    Collected(InstrCollection),
    Skipped(String),
}

// ---------------------------------------------------------------------------
// Callgrind-text parsing
// ---------------------------------------------------------------------------

/// Extracts `(client-request payload, Ir)` pairs from one callgrind text
/// profile. A benchmark part looks like:
///
/// ```text
/// part: 3
/// desc: Trigger: Client Request: benches/fib.rs::benches::bench_fib::fib_iter
/// events: Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw sysCount sysTime sysCpuTime
/// totals: 56 1 1 2 0 1 2 0 1
/// ```
///
/// The `events:` line names the columns of the following `totals:` line;
/// callgrind omits trailing zero-valued columns, so a short totals is normal
/// as long as it still reaches the Ir column. Two anomalies are skipped with a
/// warning naming `source` (the profile file) instead of recording a bogus 0
/// — which would flow into the rolling window as a 0.0 ratio: a *present*
/// token that fails to parse (malformed profile), and a totals line too short
/// to reach the Ir column at all (truncated relative to its events header).
/// Parts triggered by anything other than a client request (e.g. `Program
/// termination`) carry no payload and are skipped here.
fn parse_callgrind_parts(text: &str, source: &str) -> Vec<(String, u64)> {
    const CLIENT_REQUEST: &str = "desc: Trigger: Client Request:";
    let mut parts = Vec::new();
    // The current part's client-request payload, and the most recent
    // `events:` column list (callgrind may state it once per file or once
    // per part — track the latest either way).
    let mut pending: Option<String> = None;
    let mut columns: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix(CLIENT_REQUEST) {
            pending = Some(payload.trim().to_string());
        } else if line.starts_with("desc: Trigger:") {
            // A non-client-request trigger (e.g. `Program termination`).
            pending = None;
        } else if let Some(names) = line.strip_prefix("events:") {
            columns = names.split_whitespace().map(str::to_string).collect();
        } else if let Some(values) = line.strip_prefix("totals:") {
            let Some(payload) = pending.take() else { continue };
            let Some(ir_index) = columns.iter().position(|c| c == "Ir") else {
                eprintln!("instructions lane: part '{payload}' has no Ir column — skipped");
                continue;
            };
            let Ok(values) =
                values.split_whitespace().map(str::parse).collect::<Result<Vec<u64>, _>>()
            else {
                eprintln!(
                    "instructions lane: malformed totals for part '{payload}' in {source} — skipped"
                );
                continue;
            };
            // Callgrind omits trailing zero-valued events, so a short totals
            // is normal AS LONG AS it still reaches the Ir column. A totals
            // line too short to reach Ir (any column index) is malformed
            // relative to its events header: skip it with a warning rather
            // than record a bogus 0 — a 0 count would flow into the rolling
            // window as a 0.0 ratio (same policy as a malformed token).
            let Some(&ir) = values.get(ir_index) else {
                eprintln!(
                    "instructions lane: totals for part '{payload}' in {source} has {} field(s), \
                     too few to reach the Ir column (index {ir_index}) — skipped",
                    values.len()
                );
                continue;
            };
            parts.push((payload, ir));
        }
    }
    parts
}

/// Maps a CodSpeed bench URI to the same `(group, subject, workload)` triple
/// the walltime lane derives from the criterion tree, so both lanes produce
/// identical `criterion_results::row_key` strings for the same benchmark.
///
/// URI structure (codspeed-criterion-compat):
/// `<file path>::<module path…>::<criterion group>::<criterion bench id>`,
/// e.g. `crates/mega-evm/benches/block_bench.rs::benches::bench_block_empty_txs::block_executor_empty_txs::rex4/1_txs`.
/// The last `::`-segment is criterion's in-group bench id — `function_id`,
/// with `value_str` appended as `function_id/value` for value-parameterized
/// benches. The walltime scan splits `function_id` at the FIRST `/` into
/// `(subject, workload)` and folds `value_str` into the workload, so splitting
/// the URI's bench id at the first `/` yields the identical triple for every
/// grouped bench (`benchmark_group()` + `bench_function`/`bench_with_input`)
/// — the only style mega-evm's benches use.
///
/// Known non-parity styles (neither occurs in mega-evm's benches): bare
/// `Criterion::bench_function` at the top level (the URI has no group
/// segment, so the harness fn name would be picked up as the group) and
/// `BenchmarkId::from_parameter` (the walltime lane keys those
/// `group/group/value`, but the URI's bench id is only `value`).
///
/// Returns `None` for payloads that are not bench URIs — CodSpeed emits
/// `Metadata: codspeed-rust <version>` parts through the same client-request
/// channel.
fn uri_to_triple(uri: &str) -> Option<(String, String, String)> {
    let segments: Vec<&str> = uri.split("::").collect();
    // A bench URI always carries at least `<file>::<group>::<bench id>`;
    // metadata payloads have no `::` at all.
    if segments.len() < 3 {
        return None;
    }
    let group = segments[segments.len() - 2].to_string();
    let bench_id = segments[segments.len() - 1];
    let (subject, workload) = match bench_id.split_once('/') {
        Some((subject, workload)) => (subject.to_string(), workload.to_string()),
        None => (bench_id.to_string(), String::new()),
    };
    Some((group, subject, workload))
}

/// A callgrind profile's `creator:` line names the tool that wrote it (e.g.
/// `creator: callgrind-3.26.0.codspeed5`). We only know how to read the
/// callgrind text format; if the first `creator:` line names something else
/// (e.g. the experimental `tracegrind`, which writes a different `.tgtrace`
/// layout) that is a profile-format-drift tripwire — this returns the
/// offending creator so the caller can skip the whole file rather than
/// mis-parse it. A callgrind creator, or no creator line at all (older/partial
/// formats omit it), returns `None`: parse as usual.
fn non_callgrind_creator(text: &str) -> Option<&str> {
    let creator = text.lines().find_map(|line| line.strip_prefix("creator:"))?.trim();
    (!creator.starts_with("callgrind-")).then_some(creator)
}

/// Parses one callgrind text profile into keyed rows, skipping non-bench
/// parts (metadata, program termination). `source` names the profile in
/// anomaly warnings (malformed totals, creator drift). A whole file whose
/// `creator:` line is not callgrind is skipped (returns no rows).
pub fn parse_callgrind_rows(text: &str, source: &str) -> Vec<InstrRow> {
    if let Some(creator) = non_callgrind_creator(text) {
        eprintln!(
            "instructions lane: {source} was written by '{creator}', not callgrind — \
             file skipped (profile-format drift?)"
        );
        return Vec::new();
    }
    parse_callgrind_parts(text, source)
        .into_iter()
        .filter_map(|(uri, count)| {
            uri_to_triple(&uri).map(|(group, subject, workload)| InstrRow {
                group,
                subject,
                workload,
                count,
            })
        })
        .collect()
}

/// Folds rows into one deduplicated, deterministically-ordered list, keyed by
/// `(group, subject, workload)` (later occurrences win). Counts are
/// deterministic, so a benchmark appearing more than once (e.g. in a second
/// profile file, or a re-run target) folds to the same value.
fn dedupe_rows(rows: impl IntoIterator<Item = InstrRow>) -> Vec<InstrRow> {
    let mut by_key: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    for row in rows {
        by_key.insert((row.group, row.subject, row.workload), row.count);
    }
    by_key
        .into_iter()
        .map(|((group, subject, workload), count)| InstrRow { group, subject, workload, count })
        .collect()
}

/// Parses every `*.out` file in a profile folder and folds the parts into one
/// deduplicated, deterministically-ordered row list. The runner traces child
/// processes too (one PID-named file each); only bench-binary processes
/// contain benchmark parts, so most files contribute nothing.
pub fn scan_profile_dir(dir: &Path) -> anyhow::Result<Vec<InstrRow>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "out"))
        .collect();
    paths.sort();
    let mut rows = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)?;
        rows.extend(parse_callgrind_rows(&text, &path.display().to_string()));
    }
    Ok(dedupe_rows(rows))
}

// ---------------------------------------------------------------------------
// Ratios (same shape as criterion_results::compute_ratios, over counts)
// ---------------------------------------------------------------------------

/// One subject's instruction count and its ratio against the baseline
/// subject's count for the same `(group, workload)`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrRatioRow {
    pub subject: String,
    pub count: u64,
    pub ratio_vs_baseline: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstrWorkloadRatios {
    pub group: String,
    pub workload: String,
    pub rows: Vec<InstrRatioRow>,
}

impl crate::criterion_results::RatioInput for InstrRow {
    fn group(&self) -> &str {
        &self.group
    }
    fn subject(&self) -> &str {
        &self.subject
    }
    fn workload(&self) -> &str {
        &self.workload
    }
    fn value(&self) -> f64 {
        self.count as f64
    }
}

/// Groups `rows` by `(group, workload)` and computes each subject's count
/// ratio against that workload's `baseline_subject` row — the counts twin of
/// `criterion_results::compute_ratios` (one shared implementation), with the
/// same deterministic ordering and the same degenerate-baseline guard (a
/// zero-count baseline yields no ratio, not an inf).
pub fn compute_instr_ratios(rows: &[InstrRow], baseline_subject: &str) -> Vec<InstrWorkloadRatios> {
    crate::criterion_results::compute_ratios_by_workload(rows, baseline_subject, |r, ratio| {
        InstrRatioRow { subject: r.subject.clone(), count: r.count, ratio_vs_baseline: ratio }
    })
    .into_iter()
    .map(|(group, workload, rows)| InstrWorkloadRatios { group, workload, rows })
    .collect()
}

// ---------------------------------------------------------------------------
// Collection (subprocess side)
// ---------------------------------------------------------------------------

/// Runs the whole instructions lane for one checkout: per bench target, an
/// instrumented build (`cargo codspeed build`), a fresh profile folder, and
/// an offline runner invocation, then parses every profile written.
///
/// Returns [`CollectOutcome::Skipped`] (with the reason) when the lane is
/// skipped entirely — non-Linux host (the simulation mode needs valgrind),
/// the checkout's `Cargo.lock` missing the `codspeed-criterion-compat` bench
/// harness, or the host-provisioned tools missing — each with a one-line
/// stderr note. `os` is a parameter for testability; callers pass
/// `std::env::consts::OS`. Never fails the surrounding run: per-target
/// failures land in [`InstrCollection::failed_targets`].
pub fn collect(
    checkout: &Path,
    repo: &RepoConfig,
    cfg: &InstructionsConfig,
    profile_root: &Path,
    os: &str,
) -> CollectOutcome {
    collect_inner(checkout, repo, cfg, profile_root, os, None)
}

/// [`collect`] with the preflight probes' `PATH` injectable — a test seam:
/// overriding it with a bogus path forces the tools-missing skip hermetically,
/// without depending on what the host has installed. Production callers go
/// through [`collect`], which inherits the process environment (`None`).
fn collect_inner(
    checkout: &Path,
    repo: &RepoConfig,
    cfg: &InstructionsConfig,
    profile_root: &Path,
    os: &str,
    preflight_path: Option<&str>,
) -> CollectOutcome {
    if os != "linux" {
        let reason = format!("skipped on {os} (CodSpeed simulation mode needs Linux/valgrind)");
        eprintln!("instructions lane: {reason}");
        return CollectOutcome::Skipped(reason);
    }
    // Compat-dep probe (before the tool probes): the instrumented build has
    // nothing to trace unless the checkout resolves `codspeed-criterion-compat`.
    if let Err(reason) = compat_dep_check(checkout) {
        eprintln!("instructions lane: skipped — {reason}");
        return CollectOutcome::Skipped(reason);
    }
    // Preflight: both tools are host-provisioned, never installed by us. This
    // also records their versions and warns on a profile-format-version change.
    if let Err(reason) = preflight(preflight_path) {
        eprintln!("instructions lane: skipped — {reason}");
        return CollectOutcome::Skipped(reason);
    }

    let mut collection = InstrCollection::default();
    let mut rows = Vec::new();
    for target in &repo.bench_targets {
        match collect_target(checkout, repo, cfg, &profile_root.join(target), target) {
            Ok(target_rows) => rows.extend(target_rows),
            Err(e) => {
                eprintln!("instructions lane: target '{target}' failed: {e:#}");
                collection.failed_targets.push(target.clone());
            }
        }
    }
    collection.rows = dedupe_rows(rows);
    CollectOutcome::Collected(collection)
}

/// Reads the tracked checkout's own `Cargo.lock` and checks that it resolves
/// `codspeed-criterion-compat` — the runner's bench harness, without
/// which the instrumented build produces nothing to trace. We read the
/// checkout's lockfile (not a config knob) so an unmerged bench branch (e.g.
/// mega-evm PR #337) skips the lane gracefully instead of failing the build.
/// A missing or unreadable `Cargo.lock` is treated the same as an absent dep.
/// `Err` carries the skip reason for the caller's stderr note (and, under
/// `require_instructions`, the run's error message).
fn compat_dep_check(checkout: &Path) -> Result<(), String> {
    const DEP: &str = "codspeed-criterion-compat";
    let lock = checkout.join("Cargo.lock");
    let text = std::fs::read_to_string(&lock)
        .map_err(|e| format!("cannot read {} ({e})", lock.display()))?;
    if lock_has_package(&text, DEP) {
        Ok(())
    } else {
        Err(format!(
            "{DEP} not in {} (bench harness dep absent; is the CodSpeed bench branch merged?)",
            lock.display()
        ))
    }
}

/// Does a `Cargo.lock`'s text resolve a `[[package]]` named `pkg`? Cargo writes
/// each package's name on its own `name = "<pkg>"` line, so an exact trimmed
/// match is precise and needs no full TOML parse.
fn lock_has_package(lock_text: &str, pkg: &str) -> bool {
    let needle = format!("name = \"{pkg}\"");
    lock_text.lines().any(|line| line.trim() == needle)
}

/// Preflight the two host-provisioned tools (`codspeed --version`,
/// `cargo codspeed --version`), which we never install. Captures each version,
/// emits one visibility line naming both, and — because the callgrind parsing
/// was validated against the cargo-codspeed 5.x profile format — warns (but
/// does NOT skip: counts are still attempted) when cargo-codspeed's major
/// version is not 5. Returns the `(codspeed, cargo-codspeed)` version tokens;
/// `Err` carries the skip reason (which tool can't run, and why) for the
/// caller's stderr note. `preflight_path` overrides `PATH` for the probes —
/// a test seam.
fn preflight(preflight_path: Option<&str>) -> Result<(String, String), String> {
    let mut versions: Vec<String> = Vec::with_capacity(2);
    for (program, args, what) in [
        ("codspeed", &["--version"][..], "codspeed CLI"),
        ("cargo", &["codspeed", "--version"][..], "cargo-codspeed"),
    ] {
        let mut probe = Command::new(program);
        probe.args(args);
        if let Some(path) = preflight_path {
            probe.env("PATH", path);
        }
        // A probe is usable when its output carries a version banner, even on
        // a nonzero exit: cargo-codspeed's `--version` prints the banner to
        // stderr and exits 1 (clap's version request travels its error path;
        // verified against v5.0.1), so requiring exit 0 would skip the lane
        // on a perfectly healthy host. Only an exec failure, or versionless
        // output on a failing exit, is unusable.
        match probe.output() {
            Err(e) => return Err(format!("{what} not usable: {e}")),
            Ok(out) => {
                let combined = format!(
                    "{} {}",
                    String::from_utf8_lossy(&out.stdout).trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                let combined = combined.trim();
                match find_version_token(combined) {
                    Some(token) => versions.push(token.to_string()),
                    None if out.status.success() && !combined.is_empty() => {
                        versions.push(combined.to_string());
                    }
                    None => {
                        return Err(format!(
                            "{what} not usable: `--version` exited {} without a version banner",
                            out.status
                        ));
                    }
                }
            }
        }
    }
    let (codspeed, cargo_codspeed) = (versions.remove(0), versions.remove(0));
    eprintln!("instructions lane: codspeed {codspeed}, cargo-codspeed {cargo_codspeed}");
    if major_version(&cargo_codspeed) != Some(5) {
        eprintln!(
            "instructions lane: warning — cargo-codspeed {cargo_codspeed} is not a 5.x release; \
             callgrind parsing was validated against the 5.x profile format and may break"
        );
    }
    Ok((codspeed, cargo_codspeed))
}

/// The version token — the first whitespace-separated token whose first
/// character (after an optional leading `v`) is a digit — from a `--version`
/// line like `cargo-codspeed 5.0.1`, or `None` when nothing looks like one.
fn find_version_token(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .find(|tok| tok.trim_start_matches('v').starts_with(|c: char| c.is_ascii_digit()))
}

/// The integer major version of a version token (`5` from `5.0.1`, `6` from
/// `v6.2.0`), or `None` if it does not lead with an integer.
fn major_version(token: &str) -> Option<u64> {
    token.trim_start_matches('v').split('.').next()?.parse().ok()
}

/// Build + run + parse for one bench target. The profile folder is recreated
/// fresh per run: the runner writes PID-named files and does not clean up, so
/// stale files from a previous run would mix in.
fn collect_target(
    checkout: &Path,
    repo: &RepoConfig,
    cfg: &InstructionsConfig,
    profile_dir: &Path,
    target: &str,
) -> anyhow::Result<Vec<InstrRow>> {
    let mut build = Command::new("cargo");
    build.current_dir(checkout).args([
        "codspeed",
        "build",
        "-p",
        repo.package(),
        "--bench",
        target,
    ]);
    run_streaming(build, &format!("cargo codspeed build --bench {target}"))?;

    if profile_dir.exists() {
        std::fs::remove_dir_all(profile_dir)?;
    }
    // The runner does not create the profile folder itself.
    std::fs::create_dir_all(profile_dir)?;
    // The runner resolves --profile-folder against ITS working directory (the
    // checkout), so a relative work root must be absolutized first.
    let profile_dir = profile_dir.canonicalize()?;

    let mut run = Command::new("codspeed");
    run.current_dir(checkout)
        .args(["run", "--skip-upload", "--mode", "simulation", "--profile-folder"])
        .arg(&profile_dir)
        .args(["--", "cargo", "codspeed", "run"]);
    if let Some(filter) = &cfg.bench_filter {
        run.arg(filter);
    }
    run_streaming(run, &format!("codspeed run (target {target})"))?;

    scan_profile_dir(&profile_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criterion_results::{self, row_key};

    /// Trimmed real profile from the collection spike (two bench parts).
    const FIXTURE: &str = "\
# callgrind format
version: 1
creator: callgrind-3.26.0.codspeed5
cmd:  /scratch/codspeed-spike/target/codspeed/analysis/codspeed-spike/fib_bench
part: 3
desc: Trigger: Client Request: benches/fib_bench.rs::benches::bench_fib_small::fib_iter_small
events: Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw sysCount sysTime sysCpuTime
totals: 56 1 1 2 0 1 2 0 1

part: 4
desc: Trigger: Client Request: benches/fib_bench.rs::benches::bench_fib_large::fib_iter_large
events: Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw sysCount sysTime sysCpuTime
totals: 456 1 1 2 0 1 2 0 1
";

    #[test]
    fn test_parse_fixture_counts_and_omitted_trailing_zeros() {
        let rows = parse_callgrind_rows(FIXTURE, "fixture.out");
        assert_eq!(rows.len(), 2);
        // 9 values for 12 event names: trailing zero columns are omitted,
        // and Ir (the first column) still parses.
        assert_eq!(rows[0].count, 56);
        assert_eq!(rows[0].group, "bench_fib_small");
        assert_eq!(rows[0].subject, "fib_iter_small");
        assert_eq!(rows[0].workload, "");
        assert_eq!(rows[1].count, 456);
    }

    #[test]
    fn test_parse_skips_metadata_and_termination_parts() {
        let text = "\
# callgrind format
events: Ir Dr Dw
part: 1
desc: Trigger: Client Request: Metadata: codspeed-rust 3.0.0
totals: 11 1 1
part: 2
desc: Trigger: Client Request: benches/a.rs::benches::f::g::rex5/w1
totals: 99 1 1
part: 3
desc: Trigger: Program termination
totals: 1000 5 5
";
        let rows = parse_callgrind_rows(text, "fixture.out");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            InstrRow {
                group: "g".into(),
                subject: "rex5".into(),
                workload: "w1".into(),
                count: 99,
            }
        );
    }

    #[test]
    fn test_parse_ir_column_found_by_name_not_position() {
        // Ir is documented first, but the parser must key on the name.
        let text = "\
events: Dr Ir Dw
desc: Trigger: Client Request: b.rs::m::g::s/w
totals: 7 42
";
        let rows = parse_callgrind_rows(text, "fixture.out");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 42);
        // A totals line too short to REACH the Ir column is malformed relative
        // to its events header — the part is skipped, never recorded as 0.
        let text = "\
events: Dr Dw Ir
desc: Trigger: Client Request: b.rs::m::g::s/w
totals: 7
";
        assert!(parse_callgrind_rows(text, "fixture.out").is_empty());
    }

    #[test]
    fn test_parse_skips_part_with_malformed_totals_token() {
        // A present-but-unparseable token (in ANY column, not just Ir) marks
        // the part malformed: it must be skipped, not recorded as count 0 —
        // a 0 count for a non-baseline subject would flow into the rolling
        // window as a 0.0 ratio. Parts before and after still parse.
        let text = "\
events: Ir Dr
desc: Trigger: Client Request: b.rs::m::g::before/w
totals: 12 1
desc: Trigger: Client Request: b.rs::m::g::bad_ir/w
totals: 56garbage 1
desc: Trigger: Client Request: b.rs::m::g::bad_other/w
totals: 56 nope
desc: Trigger: Client Request: b.rs::m::g::after/w
totals: 34 1
";
        let rows = parse_callgrind_rows(text, "300.out");
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, vec!["before", "after"]);
        assert_eq!(rows[0].count, 12);
        assert_eq!(rows[1].count, 34);
    }

    #[test]
    fn test_parse_skips_part_when_totals_too_short_to_reach_ir() {
        // Ir sits at a later column here (index 2), so the guard must hold for
        // any column index, not just 0: a totals line with too few fields to
        // reach Ir is malformed and skipped with a warning — recording 0 would
        // feed a bogus 0.0 ratio into the rolling window (same policy as a
        // malformed token). Parts before/after with enough fields still parse.
        let text = "\
events: Dr Dw Ir
desc: Trigger: Client Request: b.rs::m::g::before/w
totals: 1 2 30
desc: Trigger: Client Request: b.rs::m::g::short/w
totals: 7
desc: Trigger: Client Request: b.rs::m::g::after/w
totals: 4 5 60
";
        let rows = parse_callgrind_rows(text, "short.out");
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, vec!["before", "after"]);
        // No 0-count row was recorded for the skipped short part.
        assert!(rows.iter().all(|r| r.count != 0));
        assert_eq!(rows[0].count, 30);
        assert_eq!(rows[1].count, 60);
    }

    #[test]
    fn test_parse_skips_file_with_non_callgrind_creator() {
        // The experimental tracegrind tool writes a different layout under a
        // different creator (`.tgtrace` files); treat any non-`callgrind-`
        // creator as profile-format drift and skip the whole file rather than
        // mis-parse it.
        let drift = "\
# callgrind format
creator: tracegrind-1.0.0
events: Ir Dr
desc: Trigger: Client Request: b.rs::m::g::s/w
totals: 42 1
";
        assert!(parse_callgrind_rows(drift, "drift.out").is_empty());

        // The same profile under a callgrind creator parses normally.
        let ok = drift.replace("tracegrind-1.0.0", "callgrind-3.26.0.codspeed5");
        assert_eq!(parse_callgrind_rows(&ok, "ok.out").len(), 1);

        // No creator line at all → parse as today (older/partial formats omit
        // it in some profile parts).
        let no_creator = "\
events: Ir Dr
desc: Trigger: Client Request: b.rs::m::g::s/w
totals: 42 1
";
        assert_eq!(parse_callgrind_rows(no_creator, "old.out").len(), 1);
    }

    #[test]
    fn test_scan_profile_dir_collects_across_out_files_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("100.out"), FIXTURE).unwrap();
        // A child-process profile with no benchmark parts (only termination).
        std::fs::write(
            tmp.path().join("101.out"),
            "# callgrind format\ndesc: Trigger: Program termination\nevents: Ir\ntotals: 5\n",
        )
        .unwrap();
        // A second bench-binary profile repeating one part (deterministic
        // counts: identical value) plus a new one.
        std::fs::write(
            tmp.path().join("102.out"),
            "\
events: Ir Dr
desc: Trigger: Client Request: benches/fib_bench.rs::benches::bench_fib_small::fib_iter_small
totals: 56 1
desc: Trigger: Client Request: benches/other.rs::benches::bench_other::grp::rex5/w
totals: 777 1
",
        )
        .unwrap();
        // A non-.out file must be ignored.
        std::fs::write(tmp.path().join("notes.txt"), "events: Ir\ntotals: 1\n").unwrap();

        let rows = scan_profile_dir(tmp.path()).unwrap();
        let keys: Vec<String> =
            rows.iter().map(|r| row_key(&r.group, &r.subject, &r.workload)).collect();
        assert_eq!(
            keys,
            vec!["bench_fib_large/fib_iter_large", "bench_fib_small/fib_iter_small", "grp/rex5/w"]
        );
        assert_eq!(rows.iter().find(|r| r.group == "grp").unwrap().count, 777);
        assert_eq!(rows.iter().find(|r| r.group == "bench_fib_small").unwrap().count, 56);
    }

    /// Proves row-key parity with the walltime lane: for the bench styles
    /// mega-evm uses (grouped `bench_function`, with and without a workload
    /// suffix, and value-parameterized `BenchmarkId::new`), the URI mapping
    /// and the criterion-tree scan produce the same `row_key` string.
    #[test]
    fn test_row_key_parity_with_walltime_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let write_walltime_row =
            |group: &str, dir: &str, function_id: &str, value_str: Option<&str>| {
                let mut bench_dir = tmp.path().join(group).join(dir);
                if let Some(value) = value_str {
                    bench_dir = bench_dir.join(value);
                }
                let new_dir = bench_dir.join("new");
                std::fs::create_dir_all(&new_dir).unwrap();
                std::fs::write(
                    new_dir.join("benchmark.json"),
                    serde_json::json!({
                        "group_id": group,
                        "function_id": function_id,
                        "value_str": value_str,
                    })
                    .to_string(),
                )
                .unwrap();
                std::fs::write(
                    new_dir.join("estimates.json"),
                    serde_json::json!({
                        "mean": {"point_estimate": 1000.0},
                        "std_dev": {"point_estimate": 10.0},
                    })
                    .to_string(),
                )
                .unwrap();
                std::fs::write(
                    new_dir.join("sample.json"),
                    serde_json::json!({"sampling_mode": "Auto", "iters": [1.0], "times": [1000.0]})
                        .to_string(),
                )
                .unwrap();
            };

        // The three styles, with the URI codspeed-criterion-compat emits for
        // each (`<file>::<modules…>::<group>::<function_id[/value]>`).
        // 1. `bench_function("rex4/1_txs")` — subject/workload function id.
        write_walltime_row("block_executor_empty_txs", "rex4_1_txs", "rex4/1_txs", None);
        let uri1 = "crates/mega-evm/benches/block_bench.rs::benches::bench_block_empty_txs::block_executor_empty_txs::rex4/1_txs";
        // 2. `bench_function("revm_pinned")` — bare subject, no workload.
        write_walltime_row("empty_transaction", "revm_pinned", "revm_pinned", None);
        let uri2 =
            "crates/mega-evm/benches/mega_bench.rs::benches::bench_empty_tx::empty_transaction::revm_pinned";
        // 3. `bench_with_input(BenchmarkId::new("rex5_salt", 100))` —
        //    value-parameterized; criterion folds the value into the id.
        write_walltime_row("salt_dynamic_gas", "rex5_salt", "rex5_salt", Some("100"));
        let uri3 =
            "crates/mega-evm/benches/mega_bench.rs::benches::bench_salt::salt_dynamic_gas::rex5_salt/100";

        let walltime_rows = criterion_results::scan(tmp.path()).unwrap();
        assert_eq!(walltime_rows.len(), 3);
        let walltime_keys: std::collections::BTreeSet<String> =
            walltime_rows.iter().map(|r| row_key(&r.group, &r.subject, &r.workload)).collect();

        let instr_keys: std::collections::BTreeSet<String> = [uri1, uri2, uri3]
            .iter()
            .map(|uri| {
                let (group, subject, workload) = uri_to_triple(uri).expect("bench uri maps");
                row_key(&group, &subject, &workload)
            })
            .collect();

        assert_eq!(walltime_keys, instr_keys);
        assert!(instr_keys.contains("block_executor_empty_txs/rex4/1_txs"));
        assert!(instr_keys.contains("empty_transaction/revm_pinned"));
        assert!(instr_keys.contains("salt_dynamic_gas/rex5_salt/100"));
    }

    #[test]
    fn test_compute_instr_ratios_mirrors_walltime_semantics() {
        let mk = |subject: &str, count: u64| InstrRow {
            group: "salt_dynamic_gas".into(),
            subject: subject.into(),
            workload: "sstore_100".into(),
            count,
        };
        let rows = vec![
            mk("revm_pinned", 10_000),
            mk("rex5_salt", 25_000),
            InstrRow {
                group: "oracle_real_data".into(),
                subject: "rex5_oracle".into(),
                workload: "oracle_sload_50".into(),
                count: 500,
            },
        ];
        let ratios = compute_instr_ratios(&rows, "revm_pinned");
        assert_eq!(ratios.len(), 2);
        // Baseline-less group: counts recorded, no ratio.
        assert_eq!(ratios[0].group, "oracle_real_data");
        assert_eq!(ratios[0].rows[0].ratio_vs_baseline, None);
        let salt = &ratios[1];
        assert_eq!(salt.rows[0].subject, "revm_pinned");
        assert_eq!(salt.rows[0].ratio_vs_baseline, Some(1.0));
        assert_eq!(salt.rows[1].ratio_vs_baseline, Some(2.5));
    }

    #[test]
    fn test_compute_instr_ratios_zero_baseline_yields_no_ratio() {
        let rows = vec![
            InstrRow {
                group: "g".into(),
                subject: "revm_pinned".into(),
                workload: "w".into(),
                count: 0,
            },
            InstrRow { group: "g".into(), subject: "rex5".into(), workload: "w".into(), count: 10 },
        ];
        let ratios = compute_instr_ratios(&rows, "revm_pinned");
        for row in &ratios[0].rows {
            assert_eq!(row.ratio_vs_baseline, None, "subject {}", row.subject);
        }
    }

    fn test_repo_config() -> RepoConfig {
        crate::config::Config::parse(
            r#"
[[repos]]
name = "mega-evm"
github = "megaeth-labs/mega-evm"
branch = "main"
clone_url = "https://github.com/megaeth-labs/mega-evm.git"
bench_targets = ["mega_bench"]
baseline_subject = "revm_pinned"
headline_subjects = ["rex5", "rex5_*"]
"#,
        )
        .unwrap()
        .repo("mega-evm")
        .unwrap()
        .clone()
    }

    #[test]
    fn test_collect_skipped_cleanly_on_non_linux() {
        // Must skip before touching any tool or the filesystem, and the
        // outcome must carry the reason (require_instructions surfaces it).
        let out = collect(
            Path::new("/nonexistent-checkout"),
            &test_repo_config(),
            &InstructionsConfig::default(),
            Path::new("/nonexistent-profiles"),
            "macos",
        );
        assert_eq!(
            out,
            CollectOutcome::Skipped(
                "skipped on macos (CodSpeed simulation mode needs Linux/valgrind)".to_string()
            )
        );
    }

    /// A checkout whose `Cargo.lock` DOES resolve the compat dep, so the lane
    /// clears the compat-dep gate and reaches the tool preflight.
    fn checkout_with_compat_dep() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            "[[package]]\nname = \"codspeed-criterion-compat\"\nversion = \"2.10.1\"\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn test_collect_skipped_cleanly_when_preflight_tools_missing() {
        // os IS linux and the checkout resolves the compat dep, so the lane
        // clears the compat-dep gate and reaches the version preflight — but
        // the injected PATH points nowhere, so neither `codspeed --version`
        // nor `cargo codspeed --version` can spawn (hermetic regardless of
        // what the host has installed). The lane must skip without panicking,
        // and the outcome names the unusable tool — with
        // `require_instructions = true` the pipeline turns exactly this
        // reason into the run's error after the write sequence; without it,
        // the pipeline maps the skip to a walltime-only run, whose unaffected
        // artifacts the synthetic pipeline tests pin.
        let checkout = checkout_with_compat_dep();
        let out = collect_inner(
            checkout.path(),
            &test_repo_config(),
            &InstructionsConfig::default(),
            Path::new("/nonexistent-profiles"),
            "linux",
            Some("/nonexistent-bin"),
        );
        match out {
            CollectOutcome::Skipped(reason) => {
                assert!(reason.contains("codspeed CLI not usable"), "reason: {reason}");
            }
            other => panic!("expected a tools-missing skip, got {other:?}"),
        }
    }

    #[test]
    fn test_lock_has_package_matches_only_the_package_name_line() {
        let lock = "\
[[package]]
name = \"criterion\"
version = \"0.5.1\"

[[package]]
name = \"codspeed-criterion-compat\"
version = \"2.10.1\"
";
        assert!(lock_has_package(lock, "codspeed-criterion-compat"));
        assert!(lock_has_package(lock, "criterion"));
        assert!(!lock_has_package(lock, "codspeed-divan-compat"));
        // A substring of a resolved name must not count as a match.
        assert!(!lock_has_package(lock, "codspeed-criterion"));
    }

    #[test]
    fn test_compat_dep_check_reads_the_checkout_lockfile() {
        // With the dep resolved → lane may proceed.
        let with = tempfile::tempdir().unwrap();
        std::fs::write(
            with.path().join("Cargo.lock"),
            "[[package]]\nname = \"codspeed-criterion-compat\"\nversion = \"2.10.1\"\n",
        )
        .unwrap();
        assert_eq!(compat_dep_check(with.path()), Ok(()));

        // Lockfile present but the dep absent → skip (the graceful pre-merge
        // path), with a reason naming the missing dep.
        let without = tempfile::tempdir().unwrap();
        std::fs::write(
            without.path().join("Cargo.lock"),
            "[[package]]\nname = \"criterion\"\nversion = \"0.5.1\"\n",
        )
        .unwrap();
        let reason = compat_dep_check(without.path()).unwrap_err();
        assert!(reason.contains("codspeed-criterion-compat not in"), "reason: {reason}");

        // Missing Cargo.lock (unreadable) → same skip path.
        let missing = tempfile::tempdir().unwrap();
        let reason = compat_dep_check(missing.path()).unwrap_err();
        assert!(reason.contains("cannot read"), "reason: {reason}");
    }

    #[test]
    fn test_collect_skips_before_probing_when_compat_dep_absent() {
        // A checkout whose Cargo.lock lacks the compat dep skips the lane up
        // front — the compat probe runs before the tool preflight, so this
        // skips even with the real PATH (`None` seam) and needs no host tools.
        let checkout = tempfile::tempdir().unwrap();
        std::fs::write(
            checkout.path().join("Cargo.lock"),
            "[[package]]\nname = \"criterion\"\nversion = \"0.5.1\"\n",
        )
        .unwrap();
        let out = collect_inner(
            checkout.path(),
            &test_repo_config(),
            &InstructionsConfig::default(),
            Path::new("/nonexistent-profiles"),
            "linux",
            None,
        );
        match out {
            CollectOutcome::Skipped(reason) => {
                assert!(reason.contains("codspeed-criterion-compat not in"), "reason: {reason}");
            }
            other => panic!("expected a compat-dep skip, got {other:?}"),
        }
    }

    #[test]
    fn test_version_token_and_major_version() {
        assert_eq!(find_version_token("cargo-codspeed 5.0.1"), Some("5.0.1"));
        assert_eq!(find_version_token("codspeed 4.18.0"), Some("4.18.0"));
        // A leading `v` is kept in the display token but ignored for the major.
        assert_eq!(find_version_token("v6.2.0"), Some("v6.2.0"));
        // Nothing version-shaped → None (the preflight then falls back to the
        // whole trimmed output for the visibility line on a successful exit).
        assert_eq!(find_version_token("  nightly build  "), None);

        assert_eq!(major_version("5.0.1"), Some(5));
        assert_eq!(major_version("v6.2.0"), Some(6));
        assert_eq!(major_version("18.0.0"), Some(18));
        assert_eq!(major_version("nightly"), None);
    }

    #[cfg(unix)]
    fn write_exec(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_preflight_captures_versions_and_warns_without_skipping_off_major_five() {
        // Fake host tools on an injected PATH: the preflight must capture both
        // versions and, for a non-5 cargo-codspeed major, WARN but not skip —
        // it returns the parsed version tokens (Some), so counts are still
        // attempted. Hermetic regardless of what the host has installed.
        let bin = tempfile::tempdir().unwrap();
        write_exec(&bin.path().join("codspeed"), "#!/bin/sh\necho 'codspeed 4.18.2'\n");
        write_exec(&bin.path().join("cargo"), "#!/bin/sh\necho 'cargo-codspeed 6.1.0'\n");
        let out = preflight(Some(bin.path().to_str().unwrap()));
        assert_eq!(out, Ok(("4.18.2".to_string(), "6.1.0".to_string())));
    }

    #[cfg(unix)]
    #[test]
    fn test_preflight_accepts_version_banner_on_stderr_with_nonzero_exit() {
        // Regression pin for the real cargo-codspeed 5.0.1: `cargo codspeed
        // --version` prints its banner (via clap's version-request error
        // path) and exits 1. The probe must treat that as usable — requiring
        // exit 0 skipped the lane on a healthy host.
        let bin = tempfile::tempdir().unwrap();
        write_exec(&bin.path().join("codspeed"), "#!/bin/sh\necho 'codspeed-runner 4.18.4'\n");
        write_exec(
            &bin.path().join("cargo"),
            "#!/bin/sh\necho 'cargo-codspeed 5.0.1' >&2\nexit 1\n",
        );
        let out = preflight(Some(bin.path().to_str().unwrap()));
        assert_eq!(out, Ok(("4.18.4".to_string(), "5.0.1".to_string())));
    }

    #[cfg(unix)]
    #[test]
    fn test_preflight_skips_on_nonzero_exit_without_a_version_banner() {
        // A tool that fails without ever printing a version is genuinely
        // unusable — the tolerant probe must not swallow that.
        let bin = tempfile::tempdir().unwrap();
        write_exec(&bin.path().join("codspeed"), "#!/bin/sh\necho 'broken install' >&2\nexit 1\n");
        write_exec(&bin.path().join("cargo"), "#!/bin/sh\necho 'cargo-codspeed 5.0.1'\n");
        let reason = preflight(Some(bin.path().to_str().unwrap())).unwrap_err();
        assert!(reason.contains("codspeed CLI not usable"), "reason: {reason}");
        assert!(reason.contains("without a version banner"), "reason: {reason}");
    }

    #[cfg(unix)]
    #[test]
    fn test_preflight_skips_when_a_tool_cannot_spawn() {
        // A bogus PATH means neither probe can spawn: a skip reason naming
        // the first unusable tool, no panic.
        let reason = preflight(Some("/nonexistent-bin")).unwrap_err();
        assert!(reason.contains("codspeed CLI not usable"), "reason: {reason}");
    }
}
