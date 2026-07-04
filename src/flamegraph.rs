//! Nightly flame-graph pipeline: build the `profiling`-profile
//! bench binary, sample each configured workload with the platform's profiler
//! — `perf record`/`perf script` on Linux, the built-in `sample` tool on
//! macOS (1 ms interval) — then fold and render via the `inferno` crate as a
//! library (no `inferno-*` CLI binaries to install anywhere).
//!
//! Layering mirrors `pipeline.rs`: subprocess helpers (cargo/perf/sample) are
//! thin and OS-bound; folding, demangling, differential folding, and SVG
//! rendering are pure library calls, testable anywhere.

use crate::config::{FlameWorkloadPair, RepoConfig};
use crate::git;
use crate::storage::RepoStore;
use crate::subprocess::drain_stdout_to_stderr;
use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Everything a `flamegraph` invocation produced. Archive-only: the SVGs land
/// under `flame/<day>/` and nothing is posted anywhere — no card is rendered.
#[derive(Debug)]
pub struct FlamegraphOutcome {
    /// The sha that was actually profiled (branch HEAD at checkout time).
    pub sha: String,
    pub flame_dir: PathBuf,
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

/// Folds macOS `sample` call-tree output into collapsed stack lines.
pub fn collapse_sample_output(sample_output: impl std::io::BufRead) -> anyhow::Result<Vec<u8>> {
    use inferno::collapse::sample::{Folder, Options};
    use inferno::collapse::Collapse;
    let mut folded = Vec::new();
    Folder::from(Options::default())
        .collapse(sample_output, &mut folded)
        .map_err(|e| anyhow::anyhow!("collapsing sample output: {e}"))?;
    Ok(folded)
}

/// Demangles every Rust symbol in collapsed stack lines (`f1;f2 count`) so
/// flame graphs show `mega_evm::interpreter::run` instead of `_ZN8mega_evm…`.
/// Frames that aren't mangled pass through untouched.
pub fn demangle_folded(folded: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(folded);
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let Some((stack, count)) = line.rsplit_once(' ') else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let frames: Vec<String> = stack
            .split(';')
            .map(|frame| {
                // macOS sample frames come as `module`symbol`; demangle the
                // symbol part and keep the module prefix.
                let (prefix, symbol) = match frame.rsplit_once('`') {
                    Some((module, symbol)) => (Some(module), symbol),
                    None => (None, frame),
                };
                match rustc_demangle::try_demangle(symbol) {
                    // `{:#}` drops the trailing hash disambiguator.
                    Ok(demangled) => match prefix {
                        Some(module) => format!("{module}`{demangled:#}"),
                        None => format!("{demangled:#}"),
                    },
                    Err(_) => frame.to_string(),
                }
            })
            .collect();
        out.push_str(&frames.join(";"));
        out.push(' ');
        out.push_str(count);
        out.push('\n');
    }
    out.into_bytes()
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
    if !output.status.success() {
        anyhow::bail!(
            "{} --list failed ({}):\n{}",
            bench_bin.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
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

/// Profiles one benchmark id (criterion `--profile-time` mode, `--exact` so
/// variant rows like `.../x8` are not swept in) with the platform profiler and
/// returns demangled collapsed stacks.
fn profile_workload(
    bench_bin: &Path,
    benchmark_id: &str,
    profile_secs: u64,
    scratch: &Path,
) -> anyhow::Result<Vec<u8>> {
    let folded = match std::env::consts::OS {
        "linux" => profile_with_perf(bench_bin, benchmark_id, profile_secs, scratch)?,
        "macos" => profile_with_sample(bench_bin, benchmark_id, profile_secs, scratch)?,
        other => anyhow::bail!("flamegraph profiling is not supported on {other}"),
    };
    Ok(demangle_folded(&folded))
}

/// Linux: `perf record` + `perf script` + inferno perf collapsing.
fn profile_with_perf(
    bench_bin: &Path,
    benchmark_id: &str,
    profile_secs: u64,
    scratch: &Path,
) -> anyhow::Result<Vec<u8>> {
    let perf_data = scratch.join(format!("{}.perf.data", workload_file_stem(benchmark_id)));
    let mut record_child = Command::new("perf")
        .arg("record")
        .args(["-g", "--call-graph", "dwarf,16384"])
        .arg("-o")
        .arg(&perf_data)
        .arg("--")
        .arg(bench_bin)
        .args(["--bench", "--exact", "--profile-time"])
        .arg(profile_secs.to_string())
        .arg(benchmark_id)
        // The profiled bench prints its own progress on stdout; our stdout is
        // reserved for the final JSON document, so drain it to stderr.
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn perf record (Linux only): {e}"))?;
    drain_stdout_to_stderr(&mut record_child)?;
    let status = record_child.wait()?;
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
    let folded = match collapse_perf_script(BufReader::new(stdout)) {
        Ok(folded) => folded,
        Err(e) => {
            // Don't leave a running perf script behind if folding broke.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("perf script failed for '{benchmark_id}' ({status})");
    }
    std::fs::remove_file(&perf_data).ok();
    Ok(folded)
}

/// macOS: run the bench and attach the built-in `sample` profiler to it
/// (default 1 ms interval; no root, no extra tooling), then inferno's sample
/// collapsing. Criterion's ~3 s warm-up runs the same workload, so sampling
/// from process start still measures the intended code.
fn profile_with_sample(
    bench_bin: &Path,
    benchmark_id: &str,
    profile_secs: u64,
    scratch: &Path,
) -> anyhow::Result<Vec<u8>> {
    let sample_file = scratch.join(format!("{}.sample.txt", workload_file_stem(benchmark_id)));
    let mut bench_child = Command::new(bench_bin)
        .args(["--bench", "--exact", "--profile-time"])
        .arg(profile_secs.to_string())
        .arg(benchmark_id)
        // Our stdout is reserved for the final JSON document.
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", bench_bin.display()))?;

    let mut sample_child = match Command::new("sample")
        .arg(bench_child.id().to_string())
        .arg(profile_secs.to_string())
        .args(["-mayDie", "-f"])
        .arg(&sample_file)
        // `sample` chats on stdout; keep it off our JSON channel.
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = bench_child.kill();
            let _ = bench_child.wait();
            anyhow::bail!("failed to spawn macOS sample profiler: {e}");
        }
    };

    // Drain the bench's stdout to stderr while the profiler runs alongside.
    if let Err(e) = drain_stdout_to_stderr(&mut bench_child) {
        let _ = sample_child.kill();
        let _ = sample_child.wait();
        return Err(e);
    }
    let bench_status = match bench_child.wait() {
        Ok(status) => status,
        Err(e) => {
            let _ = sample_child.kill();
            let _ = sample_child.wait();
            return Err(e.into());
        }
    };
    let sample_status = sample_child.wait()?;
    if !bench_status.success() {
        anyhow::bail!("bench run failed for '{benchmark_id}' ({bench_status})");
    }
    if !sample_status.success() {
        anyhow::bail!("sample profiler failed for '{benchmark_id}' ({sample_status})");
    }

    let file = std::fs::File::open(&sample_file)
        .map_err(|e| anyhow::anyhow!("sample output {} missing: {e}", sample_file.display()))?;
    let folded = collapse_sample_output(BufReader::new(file))?;
    std::fs::remove_file(&sample_file).ok();
    Ok(folded)
}

// ---------------------------------------------------------------------------
// Full nightly run
// ---------------------------------------------------------------------------

/// The complete `flamegraph` subcommand: checkout the tracked branch's HEAD,
/// build the bench binary once, profile every configured pair, render
/// per-workload SVGs + one differential per pair into `flame/<day>/`, and
/// prune old days. Pure archive — no card, nothing to relay.
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
    // Same exclusive lock as the per-commit pipeline: both share the checkout.
    let _lock = store.acquire_lock()?;
    let checkout = git::ensure_checkout(work_root, repo)?;

    // Nightly profiles the tracked branch's current HEAD (not a specific sha).
    git::checkout_branch_head(&checkout, repo)?;
    let sha = git::head_sha(&checkout)?;
    let meta = git::commit_meta(&checkout, &sha)?;

    // A feature id appearing in two pairs would overwrite its own `_diff.svg`
    // with a different comparison — reject the config outright.
    let mut seen_features = std::collections::BTreeSet::new();
    for pair in &flame_cfg.workloads {
        if !seen_features.insert(&pair.feature) {
            anyhow::bail!(
                "flamegraph workload feature '{}' appears in more than one pair",
                pair.feature
            );
        }
    }

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

    // Profile and render each unique id exactly once — a baseline shared by
    // several pairs is not re-profiled per pair.
    let mut folded: BTreeMap<&String, Vec<u8>> = BTreeMap::new();
    for FlameWorkloadPair { baseline, feature } in &flame_cfg.workloads {
        for id in [baseline, feature] {
            if folded.contains_key(id) {
                continue;
            }
            let data = profile_workload(&bench_bin, id, flame_cfg.profile_secs, scratch.path())?;
            let svg = flame_dir.join(format!("{}.svg", workload_file_stem(id)));
            render_flamegraph_svg(id, &data, &svg)?;
            folded.insert(id, data);
        }
    }

    for FlameWorkloadPair { baseline, feature } in &flame_cfg.workloads {
        let diff_svg = flame_dir.join(format!("{}_diff.svg", workload_file_stem(feature)));
        render_differential_svg(
            &format!("{feature} vs {baseline}"),
            &folded[baseline],
            &folded[feature],
            &diff_svg,
        )?;
    }

    let pruned = store.prune_flame_dirs(&day, flame_cfg.retention_days)?;
    Ok(FlamegraphOutcome { sha: meta.sha, flame_dir, pruned })
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
    fn test_collapse_sample_output_fixture() {
        // Minimal macOS `sample` call-tree output.
        let output = "\
Analysis of sampling mega_bench (pid 12345) every 1 millisecond
Process:         mega_bench [12345]

Call graph:
    2000 Thread_260629
      2000 start  (in dyld) + 1903  [0x1806c3f3c]
        2000 main  (in mega_bench) + 40  [0x1000e0]
          1500 evm_run  (in mega_bench) + 100  [0x100120]
            500 sstore  (in mega_bench) + 4  [0x100200]

Total number in stack (recursive counted multiple, when >=5):

Sort by top of stack, same collapsed (when >= 5):
        evm_run  (in mega_bench)        1000
";
        let folded = collapse_sample_output(Cursor::new(output.as_bytes())).unwrap();
        let text = String::from_utf8(folded).unwrap();
        // inferno folds sample frames as module`function, rooted at the thread.
        assert!(
            text.contains("mega_bench`main;mega_bench`evm_run;mega_bench`sstore 500"),
            "unexpected folded output: {text}"
        );
    }

    #[test]
    fn test_demangle_folded_rust_symbols() {
        // Bare (perf-style) and module-prefixed (macOS sample-style) frames.
        let folded = b"main;_ZN8mega_evm3run17h1234567890abcdefE;mega_bench`_ZN8mega_evm3run17h1234567890abcdefE;raw_frame 42\n".to_vec();
        let text = String::from_utf8(demangle_folded(&folded)).unwrap();
        assert_eq!(text, "main;mega_evm::run;mega_bench`mega_evm::run;raw_frame 42\n");
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
