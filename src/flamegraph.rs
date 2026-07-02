//! Nightly flame-graph pipeline (Task 2.1): build the `profiling`-profile
//! bench binary, run each configured workload under `perf record` (Linux
//! only — `perf` is shelled out, it is not a Rust library), then fold and
//! render via the `inferno` crate as a library (no `inferno-*` CLI binaries to
//! install on the server).
//!
//! Layering mirrors `pipeline.rs`: subprocess helpers (cargo/perf) are thin
//! and Linux-bound; folding, differential folding, and SVG rendering are pure
//! library calls, testable anywhere.

use crate::cards::{self, FlamegraphCardParams, RenderedCard};
use crate::config::{FlameWorkloadPair, RepoConfig};
use crate::pipeline::{commit_meta, ensure_checkout};
use crate::storage::RepoStore;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Everything a `flamegraph` invocation produced.
#[derive(Debug)]
pub struct FlamegraphOutcome {
    pub flame_dir: PathBuf,
    pub card: RenderedCard,
    /// `flame/<day>` directories removed by retention pruning.
    pub pruned: Vec<String>,
}

/// Benchmark id → safe file stem (`salt_dynamic_gas/rex5_salt/sstore_100` →
/// `salt_dynamic_gas_rex5_salt_sstore_100`).
pub fn workload_file_stem(benchmark_id: &str) -> String {
    benchmark_id.replace('/', "_")
}

// ---------------------------------------------------------------------------
// Pure library layer (inferno)
// ---------------------------------------------------------------------------

/// Folds raw `perf script` output into collapsed stack lines.
pub fn collapse_perf_script(perf_script: impl std::io::BufRead) -> anyhow::Result<Vec<u8>> {
    use inferno::collapse::perf::{Folder, Options};
    use inferno::collapse::Collapse;
    let mut folded = Vec::new();
    Folder::from(Options::default())
        .collapse(perf_script, &mut folded)
        .map_err(|e| anyhow::anyhow!("collapsing perf script output: {e}"))?;
    Ok(folded)
}

/// Renders collapsed stacks into a flame-graph SVG.
pub fn render_flamegraph_svg(title: &str, folded: &[u8], out: &Path) -> anyhow::Result<()> {
    let mut options = inferno::flamegraph::Options::default();
    options.title = title.to_string();
    options.count_name = "samples".to_string();
    let file = std::fs::File::create(out)?;
    inferno::flamegraph::from_reader(&mut options, Cursor::new(folded), file)
        .map_err(|e| anyhow::anyhow!("rendering flame graph {}: {e}", out.display()))?;
    Ok(())
}

/// Renders a differential flame graph (feature vs baseline) from two collapsed
/// stacks: red = grew vs baseline, blue = shrank.
pub fn render_differential_svg(
    title: &str,
    baseline_folded: &[u8],
    feature_folded: &[u8],
    out: &Path,
) -> anyhow::Result<()> {
    let mut diff_folded = Vec::new();
    inferno::differential::from_readers(
        inferno::differential::Options::default(),
        Cursor::new(baseline_folded),
        Cursor::new(feature_folded),
        &mut diff_folded,
    )
    .map_err(|e| anyhow::anyhow!("differential folding: {e}"))?;

    let mut options = inferno::flamegraph::Options::default();
    options.title = title.to_string();
    options.count_name = "samples".to_string();
    let file = std::fs::File::create(out)?;
    inferno::flamegraph::from_reader(&mut options, Cursor::new(&diff_folded), file)
        .map_err(|e| anyhow::anyhow!("rendering differential {}: {e}", out.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Subprocess layer (cargo + perf; the perf parts are Linux-only)
// ---------------------------------------------------------------------------

/// Builds the bench binary without running it and returns its path, parsed
/// from cargo's JSON messages.
pub fn build_bench_binary(
    checkout: &Path,
    repo: &RepoConfig,
    bench_target: &str,
) -> anyhow::Result<PathBuf> {
    let output = Command::new("cargo")
        .current_dir(checkout)
        .args([
            "bench",
            "-p",
            repo.package(),
            "--bench",
            bench_target,
            "--profile",
            "profiling",
            "--no-run",
            "--message-format=json",
        ])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn cargo bench --no-run: {e}"))?;
    if !output.status.success() {
        anyhow::bail!("cargo bench --no-run failed ({})", output.status);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    find_bench_executable(&stdout, bench_target).ok_or_else(|| {
        anyhow::anyhow!("no executable for bench target '{bench_target}' in cargo output")
    })
}

/// Extracts the bench executable path from `--message-format=json` output.
fn find_bench_executable(cargo_json_lines: &str, bench_target: &str) -> Option<PathBuf> {
    for line in cargo_json_lines.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if msg["reason"] != "compiler-artifact" {
            continue;
        }
        let target = &msg["target"];
        let is_bench =
            target["kind"].as_array().is_some_and(|kinds| kinds.iter().any(|k| k == "bench"));
        if is_bench && target["name"] == bench_target {
            if let Some(exe) = msg["executable"].as_str() {
                return Some(PathBuf::from(exe));
            }
        }
    }
    None
}

/// Asserts the exact benchmark id exists (criterion `--list`), catching typos
/// and row/variant-order mistakes before a long profiling run.
fn verify_benchmark_id(bench_bin: &Path, benchmark_id: &str) -> anyhow::Result<()> {
    let output = Command::new(bench_bin)
        .args(["--bench", "--list"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run {} --list: {e}", bench_bin.display()))?;
    let listing = String::from_utf8_lossy(&output.stdout);
    // criterion lists ids as `<id>: benchmark` lines.
    if !listing.lines().any(|l| l.trim_end_matches(": benchmark").trim() == benchmark_id) {
        anyhow::bail!(
            "benchmark id '{benchmark_id}' not found in `--list` output of {} — \
             check the id's row/variant order",
            bench_bin.display()
        );
    }
    Ok(())
}

/// Profiles one benchmark id under `perf record` (criterion `--profile-time`
/// mode, `--exact` so variant rows like `.../x8` are not swept in) and returns
/// the collapsed stacks.
fn profile_workload(
    bench_bin: &Path,
    benchmark_id: &str,
    profile_secs: u64,
    scratch: &Path,
) -> anyhow::Result<Vec<u8>> {
    let perf_data = scratch.join(format!("{}.perf.data", workload_file_stem(benchmark_id)));
    let status = Command::new("perf")
        .arg("record")
        .args(["-g", "--call-graph", "dwarf,16384"])
        .arg("-o")
        .arg(&perf_data)
        .arg("--")
        .arg(bench_bin)
        .args(["--bench", "--exact", "--profile-time"])
        .arg(profile_secs.to_string())
        .arg(benchmark_id)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn perf record (Linux only): {e}"))?;
    if !status.success() {
        anyhow::bail!("perf record failed for '{benchmark_id}' ({status})");
    }

    let mut child = Command::new("perf")
        .arg("script")
        .arg("-i")
        .arg(&perf_data)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn perf script: {e}"))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let folded = collapse_perf_script(BufReader::new(stdout))?;
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("perf script failed for '{benchmark_id}' ({status})");
    }
    std::fs::remove_file(&perf_data).ok();
    Ok(folded)
}

// ---------------------------------------------------------------------------
// Full nightly run
// ---------------------------------------------------------------------------

/// The complete `flamegraph` subcommand: checkout the tracked branch's HEAD,
/// build the bench binary once, profile every configured pair, render
/// per-workload SVGs + one differential per pair into `flame/<day>/`, prune
/// old days, and render the flamegraph card.
pub fn run_flamegraph_pipeline(
    repo: &RepoConfig,
    data_root: &Path,
    work_root: &Path,
) -> anyhow::Result<FlamegraphOutcome> {
    let Some(flame_cfg) = &repo.flamegraph else {
        anyhow::bail!("repo '{}' has no [repos.flamegraph] config", repo.name);
    };
    if flame_cfg.workloads.is_empty() {
        anyhow::bail!("repo '{}' has an empty flamegraph workload list", repo.name);
    }
    let store = RepoStore::new(data_root, &repo.name);
    let checkout = ensure_checkout(work_root, repo)?;

    // Nightly profiles the tracked branch's current HEAD (not a specific sha).
    crate::pipeline::checkout_branch_head(&checkout, repo)?;
    let sha = crate::pipeline::head_sha(&checkout)?;
    let meta = commit_meta(&checkout, &sha)?;

    let bench_bin = build_bench_binary(&checkout, repo, &flame_cfg.bench_target)?;
    for pair in &flame_cfg.workloads {
        verify_benchmark_id(&bench_bin, &pair.baseline)?;
        verify_benchmark_id(&bench_bin, &pair.feature)?;
    }

    let now = time::OffsetDateTime::now_utc();
    let day = format!("{:04}{:02}{:02}", now.year(), now.month() as u8, now.day());
    let flame_dir = store.flame_dir(&day);
    std::fs::create_dir_all(&flame_dir)?;
    let scratch = tempfile::tempdir()?;

    let mut card_workloads = Vec::new();
    for FlameWorkloadPair { baseline, feature } in &flame_cfg.workloads {
        let baseline_folded =
            profile_workload(&bench_bin, baseline, flame_cfg.profile_secs, scratch.path())?;
        let feature_folded =
            profile_workload(&bench_bin, feature, flame_cfg.profile_secs, scratch.path())?;

        let baseline_svg = flame_dir.join(format!("{}.svg", workload_file_stem(baseline)));
        render_flamegraph_svg(baseline, &baseline_folded, &baseline_svg)?;
        let feature_svg = flame_dir.join(format!("{}.svg", workload_file_stem(feature)));
        render_flamegraph_svg(feature, &feature_folded, &feature_svg)?;
        let diff_svg = flame_dir.join(format!("{}_diff.svg", workload_file_stem(feature)));
        render_differential_svg(
            &format!("{feature} vs {baseline}"),
            &baseline_folded,
            &feature_folded,
            &diff_svg,
        )?;

        card_workloads.push((feature.clone(), feature_svg, Some(diff_svg)));
        card_workloads.push((baseline.clone(), baseline_svg, None));
    }

    let pruned = store.prune_flame_dirs(&day, flame_cfg.retention_days)?;
    let card = cards::render_flamegraph_card(&FlamegraphCardParams {
        repo_name: &repo.name,
        github: &repo.github,
        sha: &meta.sha,
        day: &day,
        workloads: card_workloads,
    })?;

    Ok(FlamegraphOutcome { flame_dir, card, pruned })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workload_file_stem() {
        assert_eq!(
            workload_file_stem("salt_dynamic_gas/rex5_salt/sstore_100"),
            "salt_dynamic_gas_rex5_salt_sstore_100"
        );
    }

    #[test]
    fn test_collapse_perf_script_fixture() {
        // Minimal but well-formed `perf script` output: two samples of the
        // same two-frame stack (leaf first, as perf prints them).
        let script = "\
mega_bench 1234 100.000001: 250000 cycles:\n\
\t            55a8f0 evm_run (/x/mega_bench)\n\
\t            55a900 main (/x/mega_bench)\n\
\n\
mega_bench 1234 100.000002: 250000 cycles:\n\
\t            55a8f0 evm_run (/x/mega_bench)\n\
\t            55a900 main (/x/mega_bench)\n\
\n";
        let folded = collapse_perf_script(Cursor::new(script.as_bytes())).unwrap();
        let text = String::from_utf8(folded).unwrap();
        // inferno roots stacks at the process name and weighs by event period.
        assert!(
            text.contains("mega_bench;main;evm_run 500000"),
            "unexpected folded output: {text}"
        );
    }

    #[test]
    fn test_render_flamegraph_and_differential_svgs() {
        let tmp = tempfile::tempdir().unwrap();
        let baseline = b"main;evm_run 90\nmain;evm_run;sstore 10\n".to_vec();
        let feature =
            b"main;evm_run 90\nmain;evm_run;sstore 40\nmain;evm_run;salt_lookup 15\n".to_vec();

        let svg = tmp.path().join("feature.svg");
        render_flamegraph_svg("feature", &feature, &svg).unwrap();
        let svg_text = std::fs::read_to_string(&svg).unwrap();
        assert!(svg_text.contains("<svg"));
        assert!(svg_text.contains("salt_lookup"));

        let diff = tmp.path().join("feature_diff.svg");
        render_differential_svg("feature vs baseline", &baseline, &feature, &diff).unwrap();
        let diff_text = std::fs::read_to_string(&diff).unwrap();
        assert!(diff_text.contains("<svg"));
        assert!(diff_text.contains("sstore"));
    }

    #[test]
    fn test_find_bench_executable_from_cargo_json() {
        let lines = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"mega-evm","kind":["lib"]},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"mega_bench","kind":["bench"]},"executable":"/t/profiling/deps/mega_bench-abc123"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(
            find_bench_executable(lines, "mega_bench"),
            Some(PathBuf::from("/t/profiling/deps/mega_bench-abc123"))
        );
        assert_eq!(find_bench_executable(lines, "other_bench"), None);
    }
}
