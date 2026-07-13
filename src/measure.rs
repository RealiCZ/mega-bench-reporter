//! Standalone `measure` subcommand: run walltime and/or instructions
//! collection against a caller-supplied checkout and emit one JSON document
//! on stdout. Parameterized exposure of the existing criterion scan and
//! instructions-lane collect/parse — no new measurement logic, no git ops.

use crate::criterion_results::{self, row_key, Row};
use crate::instructions::{self, CollectOutcome, CollectRequest, InstrRow};
use crate::pipeline;
use crate::subprocess::run_cmd;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Inputs for one `measure` invocation.
#[derive(Debug, Clone)]
pub struct MeasureRequest {
    /// Existing checkout directory — the caller owns the worktree; this
    /// module never runs git.
    pub checkout: PathBuf,
    pub package: String,
    pub bench_targets: Vec<String>,
    pub instructions: bool,
    pub walltime: bool,
    /// Optional criterion / codspeed bench filter. Applied to the
    /// instructions lane always; for walltime it is forwarded as a criterion
    /// free-arg filter when the lane runs.
    pub bench_filter: Option<String>,
    /// Host OS; production callers pass `std::env::consts::OS`. Injected so
    /// tests can exercise the non-Linux `--instructions` error path.
    pub os: String,
}

/// Per-row metrics. Fields omitted from JSON when that lane did not run
/// (or produced no value for the row).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeasureRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instr_count: Option<u64>,
}

/// Provenance metadata for the measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MeasureMeta {
    /// `rustc -V` stdout (trimmed).
    pub rustc: String,
    /// See [`profile_fingerprint`] for the exact recipe.
    pub profile_fingerprint: String,
}

/// The single JSON document printed on stdout.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeasureOutput {
    /// Pipeline-compatible row keys → metrics.
    pub rows: BTreeMap<String, MeasureRow>,
    pub meta: MeasureMeta,
}

/// Validate request shape before any work. Shared by the CLI and unit tests.
pub fn validate_request(req: &MeasureRequest) -> anyhow::Result<()> {
    if !req.instructions && !req.walltime {
        anyhow::bail!("at least one of --instructions / --walltime is required");
    }
    if req.bench_targets.is_empty() {
        anyhow::bail!("at least one --bench-target is required");
    }
    if !req.checkout.is_dir() {
        anyhow::bail!("checkout is not a directory: {}", req.checkout.display());
    }
    Ok(())
}

/// Run the requested lanes against `req.checkout` and assemble the JSON
/// payload. Logs go to stderr; the caller serializes the return value to
/// stdout.
pub fn measure(req: &MeasureRequest) -> anyhow::Result<MeasureOutput> {
    validate_request(req)?;

    let rustc = run_cmd(Command::new("rustc").arg("-V"), "rustc -V")?;
    let profile_fingerprint = profile_fingerprint(&req.checkout, &rustc)?;

    let walltime_rows = if req.walltime { Some(run_walltime(req)?) } else { None };
    let instr_rows = if req.instructions { Some(run_instructions(req)?) } else { None };

    Ok(assemble(walltime_rows.as_deref(), instr_rows.as_deref(), &rustc, &profile_fingerprint))
}

/// Pure assembly of the measure JSON from already-collected lane data.
/// Integration tests drive this against fixtures without running cargo or
/// codspeed.
pub fn assemble(
    walltime: Option<&[Row]>,
    instructions: Option<&[InstrRow]>,
    rustc: &str,
    profile_fingerprint: &str,
) -> MeasureOutput {
    let mut rows: BTreeMap<String, MeasureRow> = BTreeMap::new();

    if let Some(wt) = walltime {
        for r in wt {
            let key = row_key(&r.group, &r.subject, &r.workload);
            rows.entry(key).or_insert(MeasureRow { ns: None, instr_count: None }).ns =
                Some(r.mean_ns);
        }
    }
    if let Some(instr) = instructions {
        for r in instr {
            let key = row_key(&r.group, &r.subject, &r.workload);
            rows.entry(key).or_insert(MeasureRow { ns: None, instr_count: None }).instr_count =
                Some(r.count);
        }
    }

    MeasureOutput {
        rows,
        meta: MeasureMeta {
            rustc: rustc.to_string(),
            profile_fingerprint: profile_fingerprint.to_string(),
        },
    }
}

/// Profile fingerprint recipe (stable across hosts/toolchains for the same
/// inputs):
///
/// 1. Capture `rustc -V` stdout, trimmed (passed in as `rustc`).
/// 2. Read the checkout root `Cargo.toml`. Extract the raw body of the
///    `[profile.release]` and `[profile.bench]` sections (text after the
///    header line up to — but not including — the next line that starts a
///    new `[...]` table header). A missing section contributes the empty
///    string.
/// 3. Compute a 64-bit FNV-1a hash over
///    `release_body || 0x00 || bench_body` (UTF-8 bytes).
/// 4. Fingerprint string = `"{rustc}|{hash:016x}"`.
pub fn profile_fingerprint(checkout: &Path, rustc: &str) -> anyhow::Result<String> {
    let cargo_toml = checkout.join("Cargo.toml");
    let text = if cargo_toml.is_file() {
        std::fs::read_to_string(&cargo_toml)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", cargo_toml.display()))?
    } else {
        String::new()
    };
    let release_body = extract_toml_section(&text, "profile.release");
    let bench_body = extract_toml_section(&text, "profile.bench");
    let mut bytes = Vec::with_capacity(release_body.len() + 1 + bench_body.len());
    bytes.extend_from_slice(release_body.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(bench_body.as_bytes());
    let hash = fnv1a64(&bytes);
    Ok(format!("{rustc}|{hash:016x}"))
}

/// Body of a TOML table named `section` (e.g. `"profile.release"`), without
/// the `[section]` header. Empty string when the section is absent.
fn extract_toml_section(text: &str, section: &str) -> String {
    let header = format!("[{section}]");
    let mut lines = text.lines();
    // Find the exact header line (trimmed).
    loop {
        match lines.next() {
            Some(line) if line.trim() == header => break,
            Some(_) => continue,
            None => return String::new(),
        }
    }
    let mut body = String::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    // Trim a single trailing newline for stability (bodies never keep a
    // final empty line that depends on whether the file ends with \n).
    while body.ends_with('\n') {
        body.pop();
    }
    body
}

/// FNV-1a 64-bit — stable, dependency-free hash for the fingerprint.
fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn run_walltime(req: &MeasureRequest) -> anyhow::Result<Vec<Row>> {
    let criterion_dir = criterion_results::criterion_dir_for(&req.checkout);
    // Fresh tree so a previous run's results don't leak in.
    if criterion_dir.exists() {
        std::fs::remove_dir_all(&criterion_dir)?;
    }
    let mut failed = Vec::new();
    for target in &req.bench_targets {
        if let Err(e) = pipeline::bench_target(
            &req.checkout,
            &req.package,
            target,
            None,
            req.bench_filter.as_deref(),
        ) {
            eprintln!("measure walltime: target '{target}' failed: {e:#}");
            failed.push(target.clone());
        }
    }
    if failed.len() == req.bench_targets.len() {
        anyhow::bail!(
            "all {} walltime bench target(s) failed — no data to report",
            req.bench_targets.len()
        );
    }
    if !criterion_dir.is_dir() {
        anyhow::bail!(
            "walltime benches finished but no criterion tree at {}",
            criterion_dir.display()
        );
    }
    criterion_results::scan(&criterion_dir)
}

fn run_instructions(req: &MeasureRequest) -> anyhow::Result<Vec<InstrRow>> {
    // Profile root lives under the checkout's target/ so concurrent measure
    // calls on different checkouts don't collide; recreated per target inside
    // collect_target.
    let profile_root = req.checkout.join("target").join("_measure_instr_profiles");
    let outcome = instructions::collect_with(
        &CollectRequest {
            checkout: &req.checkout,
            package: &req.package,
            bench_targets: &req.bench_targets,
            bench_filter: req.bench_filter.as_deref(),
            profile_root: &profile_root,
            os: &req.os,
        },
        None,
    );
    match outcome {
        // Unlike the run pipeline's graceful skip, measure is an explicit
        // request: a skip is a hard error with the same reason text.
        CollectOutcome::Skipped(reason) => {
            anyhow::bail!("--instructions failed: {reason}");
        }
        CollectOutcome::Collected(c) => {
            if !c.failed_targets.is_empty() {
                anyhow::bail!(
                    "--instructions: {} target(s) failed: {}",
                    c.failed_targets.len(),
                    c.failed_targets.join(", ")
                );
            }
            Ok(c.rows)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criterion_results::Row;
    use crate::instructions::InstrRow;

    fn write_criterion_row(root: &Path, group: &str, function_id: &str, mean_ns: f64) {
        let dir_name = function_id.replace('/', "_");
        let new_dir = root.join(group).join(dir_name).join("new");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(
            new_dir.join("benchmark.json"),
            serde_json::json!({ "group_id": group, "function_id": function_id }).to_string(),
        )
        .unwrap();
        std::fs::write(
            new_dir.join("estimates.json"),
            serde_json::json!({
                "mean": { "point_estimate": mean_ns },
                "std_dev": { "point_estimate": mean_ns * 0.01 },
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            new_dir.join("sample.json"),
            serde_json::json!({
                "sampling_mode": "Auto",
                "iters": [1.0, 1.0],
                "times": [mean_ns, mean_ns],
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn test_validate_requires_a_lane_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let req = MeasureRequest {
            checkout: tmp.path().to_path_buf(),
            package: "pkg".into(),
            bench_targets: vec!["t".into()],
            instructions: false,
            walltime: false,
            bench_filter: None,
            os: "linux".into(),
        };
        let err = validate_request(&req).unwrap_err().to_string();
        assert!(err.contains("--instructions") && err.contains("--walltime"), "{err}");
    }

    #[test]
    fn test_validate_rejects_missing_checkout() {
        let req = MeasureRequest {
            checkout: PathBuf::from("/nonexistent-measure-checkout"),
            package: "pkg".into(),
            bench_targets: vec!["t".into()],
            instructions: false,
            walltime: true,
            bench_filter: None,
            os: "linux".into(),
        };
        let err = validate_request(&req).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn test_assemble_walltime_fixture_omits_instr_count() {
        let tmp = tempfile::tempdir().unwrap();
        write_criterion_row(tmp.path(), "salt_dynamic_gas", "rex5_salt/sstore_100", 14_000.0);
        write_criterion_row(tmp.path(), "empty_transaction", "revm_pinned", 8_000.0);
        let rows = criterion_results::scan(tmp.path()).unwrap();
        let out = assemble(Some(&rows), None, "rustc 1.89.0", "rustc 1.89.0|deadbeef");

        let json = serde_json::to_value(&out).unwrap();
        let salt = &json["rows"]["salt_dynamic_gas/rex5_salt/sstore_100"];
        assert_eq!(salt["ns"], 14_000.0);
        assert!(salt.get("instr_count").is_none(), "instr_count must be omitted: {salt}");
        let empty = &json["rows"]["empty_transaction/revm_pinned"];
        assert_eq!(empty["ns"], 8_000.0);
        assert!(empty.get("instr_count").is_none());
        assert_eq!(json["meta"]["rustc"], "rustc 1.89.0");
        assert_eq!(json["meta"]["profile_fingerprint"], "rustc 1.89.0|deadbeef");
    }

    #[test]
    fn test_assemble_instructions_fixture_omits_ns() {
        // Same fixture shape as instructions::tests::FIXTURE parts.
        let instr = vec![
            InstrRow {
                group: "bench_fib_small".into(),
                subject: "fib_iter_small".into(),
                workload: String::new(),
                count: 56,
            },
            InstrRow {
                group: "grp".into(),
                subject: "rex5".into(),
                workload: "w".into(),
                count: 777,
            },
        ];
        let out = assemble(None, Some(&instr), "rustc 1.89.0", "fp");
        let json = serde_json::to_value(&out).unwrap();
        let fib = &json["rows"]["bench_fib_small/fib_iter_small"];
        assert_eq!(fib["instr_count"], 56);
        assert!(fib.get("ns").is_none(), "ns must be omitted: {fib}");
        assert_eq!(json["rows"]["grp/rex5/w"]["instr_count"], 777);
    }

    #[test]
    fn test_assemble_both_lanes_merge_on_row_key() {
        let walltime = vec![Row {
            group: "g".into(),
            subject: "s".into(),
            workload: "w".into(),
            mean_ns: 100.0,
            std_dev_ns: 1.0,
            samples_ns: vec![100.0],
        }];
        let instr = vec![InstrRow {
            group: "g".into(),
            subject: "s".into(),
            workload: "w".into(),
            count: 42,
        }];
        let out = assemble(Some(&walltime), Some(&instr), "rustc", "fp");
        let json = serde_json::to_value(&out).unwrap();
        let row = &json["rows"]["g/s/w"];
        assert_eq!(row["ns"], 100.0);
        assert_eq!(row["instr_count"], 42);
        // Golden full document shape (stable key order via BTreeMap).
        let pretty = serde_json::to_string_pretty(&out).unwrap();
        let expected = r#"{
  "rows": {
    "g/s/w": {
      "ns": 100.0,
      "instr_count": 42
    }
  },
  "meta": {
    "rustc": "rustc",
    "profile_fingerprint": "fp"
  }
}"#;
        assert_eq!(pretty, expected);
    }

    #[test]
    fn test_profile_fingerprint_hashes_release_and_bench_sections() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"
[package]
name = "x"
version = "0.1.0"

[profile.release]
opt-level = 3
lto = "thin"

[profile.bench]
inherits = "release"
"#,
        )
        .unwrap();
        let fp = profile_fingerprint(tmp.path(), "rustc 1.89.0 (abc)").unwrap();
        assert!(fp.starts_with("rustc 1.89.0 (abc)|"), "{fp}");
        // Stable across calls.
        let fp2 = profile_fingerprint(tmp.path(), "rustc 1.89.0 (abc)").unwrap();
        assert_eq!(fp, fp2);
        // Changing a profile section changes the hash.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"
[package]
name = "x"
version = "0.1.0"

[profile.release]
opt-level = 2

[profile.bench]
inherits = "release"
"#,
        )
        .unwrap();
        let fp3 = profile_fingerprint(tmp.path(), "rustc 1.89.0 (abc)").unwrap();
        assert_ne!(fp, fp3);
        // Same rustc prefix, different hash suffix.
        assert!(fp3.starts_with("rustc 1.89.0 (abc)|"));
    }

    #[test]
    fn test_profile_fingerprint_empty_sections_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let fp = profile_fingerprint(tmp.path(), "rustc 1.0").unwrap();
        // Empty release + empty bench → hash of [0x00] only.
        let expected_hash = fnv1a64(&[0]);
        assert_eq!(fp, format!("rustc 1.0|{expected_hash:016x}"));
    }

    #[test]
    fn test_measure_instructions_errors_on_non_linux() {
        let tmp = tempfile::tempdir().unwrap();
        // Minimal Cargo.toml so fingerprint / validate succeed.
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let req = MeasureRequest {
            checkout: tmp.path().to_path_buf(),
            package: "x".into(),
            bench_targets: vec!["t".into()],
            instructions: true,
            walltime: false,
            bench_filter: None,
            os: "macos".into(),
        };
        let err = measure(&req).unwrap_err().to_string();
        assert!(err.contains("--instructions failed"), "{err}");
        assert!(err.contains("macos"), "{err}");
        assert!(err.contains("Linux/valgrind"), "{err}");
    }

    #[test]
    fn test_extract_toml_section_stops_at_next_header() {
        let text = "\
[profile.release]
opt-level = 3

[profile.release.package.foo]
opt-level = 1

[profile.bench]
debug = true
";
        let release = extract_toml_section(text, "profile.release");
        // Trailing blank line before the next header is stripped.
        assert_eq!(release, "opt-level = 3");
        assert!(!release.contains("package.foo"));
        let bench = extract_toml_section(text, "profile.bench");
        assert_eq!(bench, "debug = true");
        assert_eq!(extract_toml_section(text, "profile.dev"), "");
    }
}
