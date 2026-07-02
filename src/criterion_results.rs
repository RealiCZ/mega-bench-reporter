//! Reads criterion's own `target/criterion/<group>/<row>/new/{benchmark.json,estimates.json,sample.json}`
//! tree directly — precise, structured, and already written as a side effect of
//! any `cargo bench` run regardless of `--output-format` (no text parsing needed).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct BenchmarkJson {
    group_id: String,
    /// `None` for bare `bench_function` benches and for
    /// `BenchmarkId::from_parameter` rows.
    #[serde(default)]
    function_id: Option<String>,
    /// Set for value-parameterized benches (`BenchmarkId::new(f, value)`),
    /// which nest one directory level deeper; folded into the workload.
    #[serde(default)]
    value_str: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Estimate {
    point_estimate: f64,
}

#[derive(Debug, Deserialize)]
struct EstimatesJson {
    mean: Estimate,
    std_dev: Estimate,
}

#[derive(Debug, Deserialize)]
struct SampleJson {
    iters: Vec<f64>,
    times: Vec<f64>,
}

/// One `(group, subject, workload)` row's summary stats and raw per-sample
/// per-call times (ns), the latter for violin/distribution rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub group: String,
    pub subject: String,
    pub workload: String,
    pub mean_ns: f64,
    pub std_dev_ns: f64,
    pub samples_ns: Vec<f64>,
}

/// Splits criterion's `function_id` (e.g. `"revm_pinned/sstore_100"` or just
/// `"revm_pinned"`) into `(subject, workload)`. A bare function_id (no `/`) has
/// an empty workload — matches on group+workload compare it against other bare
/// subjects in the same group.
fn split_function_id(function_id: &str) -> (String, String) {
    match function_id.split_once('/') {
        Some((subject, workload)) => (subject.to_string(), workload.to_string()),
        None => (function_id.to_string(), String::new()),
    }
}

/// Scans a `target/criterion` directory and returns every row it can fully
/// parse. Criterion lays benchmarks out at three possible depths, all handled:
/// `<name>/new` (bare `bench_function`), `<group>/<function>/new`, and
/// `<group>/<function>/<value>/new` (value-parameterized) — the same function
/// can have both of the last two at once. A directory missing any of the three
/// JSON files (e.g. an in-progress or profile-time-only run) is skipped;
/// a directory whose JSON fails to parse is skipped with a stderr warning —
/// neither aborts the scan.
pub fn scan(criterion_dir: &Path) -> anyhow::Result<Vec<Row>> {
    let mut rows = Vec::new();
    if !criterion_dir.is_dir() {
        anyhow::bail!("{} is not a directory", criterion_dir.display());
    }
    for group_entry in std::fs::read_dir(criterion_dir)? {
        let group_entry = group_entry?;
        if !group_entry.file_type()?.is_dir() {
            continue;
        }
        // `report/` is criterion's own HTML+SVG output dir, not a benchmark.
        if group_entry.file_name() == "report" {
            continue;
        }
        // A bare `bench_function` writes its data directly at the top level.
        if let Some(row) = read_row(&group_entry.path())? {
            rows.push(row);
        }
        for dir_entry in std::fs::read_dir(group_entry.path())? {
            let dir_entry = dir_entry?;
            if !dir_entry.file_type()?.is_dir() || dir_entry.file_name() == "report" {
                continue;
            }
            if let Some(row) = read_row(&dir_entry.path())? {
                rows.push(row);
            }
            // Value-parameterized benches nest one level deeper
            // (`<group>/<function>/<value>/new`) and can coexist with the
            // `<group>/<function>/new` layout above. Criterion's own data
            // dirs (`new`, `base`, `change`) contain no nested benchmark.json
            // and fall out of read_row as None.
            for value_entry in std::fs::read_dir(dir_entry.path())? {
                let value_entry = value_entry?;
                if !value_entry.file_type()?.is_dir() || value_entry.file_name() == "report" {
                    continue;
                }
                if let Some(row) = read_row(&value_entry.path())? {
                    rows.push(row);
                }
            }
        }
    }
    Ok(rows)
}

fn read_row(bench_dir: &Path) -> anyhow::Result<Option<Row>> {
    let new_dir = bench_dir.join("new");
    let (benchmark_path, estimates_path, sample_path) = (
        new_dir.join("benchmark.json"),
        new_dir.join("estimates.json"),
        new_dir.join("sample.json"),
    );
    if !benchmark_path.is_file() || !estimates_path.is_file() || !sample_path.is_file() {
        return Ok(None);
    }
    // A malformed row (unexpected schema) is warned about and skipped rather
    // than aborting the whole scan and losing every other row's data.
    macro_rules! parse_or_skip {
        ($ty:ty, $path:expr) => {
            match serde_json::from_str::<$ty>(&std::fs::read_to_string(&$path)?) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("skipping unparseable {}: {e}", $path.display());
                    return Ok(None);
                }
            }
        };
    }
    let benchmark = parse_or_skip!(BenchmarkJson, benchmark_path);
    let estimates = parse_or_skip!(EstimatesJson, estimates_path);
    let sample = parse_or_skip!(SampleJson, sample_path);

    // Bare `bench_function` / `BenchmarkId::from_parameter` rows have no
    // function_id; the group name doubles as the subject so the row still
    // gets a stable, non-empty key.
    let (subject, mut workload) = match &benchmark.function_id {
        Some(function_id) => split_function_id(function_id),
        None => (benchmark.group_id.clone(), String::new()),
    };
    if let Some(value) = &benchmark.value_str {
        workload = if workload.is_empty() { value.clone() } else { format!("{workload}/{value}") };
    }
    let samples_ns: Vec<f64> =
        sample.iters.iter().zip(sample.times.iter()).map(|(iters, ns)| ns / iters).collect();

    Ok(Some(Row {
        group: benchmark.group_id,
        subject,
        workload,
        mean_ns: estimates.mean.point_estimate,
        std_dev_ns: estimates.std_dev.point_estimate,
        samples_ns,
    }))
}

/// One workload's ratio table: every subject's `mean_ns` and its ratio against
/// the configured baseline subject for the same `(group, workload)`. `None` if
/// the group/workload has no baseline row to compare against (skipped, not an
/// error).
#[derive(Debug, Clone, PartialEq)]
pub struct RatioRow {
    pub subject: String,
    pub mean_ns: f64,
    pub ratio_vs_baseline: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadRatios {
    pub group: String,
    pub workload: String,
    pub rows: Vec<RatioRow>,
}

/// Groups `rows` by `(group, workload)` and computes each subject's ratio
/// against that workload's `baseline_subject` row. Ordering is deterministic
/// (BTreeMap) so digest tables and tests aren't flaky on directory-read order.
pub fn compute_ratios(rows: &[Row], baseline_subject: &str) -> Vec<WorkloadRatios> {
    let mut by_key: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
    for row in rows {
        by_key.entry((row.group.clone(), row.workload.clone())).or_default().push(row);
    }
    by_key
        .into_iter()
        .map(|((group, workload), group_rows)| {
            // A degenerate baseline (zero/NaN estimate) would poison every
            // downstream median/chart with inf/NaN — treat it as absent.
            let baseline_ns = group_rows
                .iter()
                .find(|r| r.subject == baseline_subject)
                .map(|r| r.mean_ns)
                .filter(|ns| ns.is_finite() && *ns > 0.0);
            let mut ratio_rows: Vec<RatioRow> = group_rows
                .iter()
                .map(|r| RatioRow {
                    subject: r.subject.clone(),
                    mean_ns: r.mean_ns,
                    ratio_vs_baseline: baseline_ns
                        .map(|b| r.mean_ns / b)
                        .filter(|ratio| ratio.is_finite()),
                })
                .collect();
            ratio_rows.sort_by(|a, b| a.subject.cmp(&b.subject));
            WorkloadRatios { group, workload, rows: ratio_rows }
        })
        .collect()
}

/// Convenience: full_id-style key for a row, e.g. `"salt_dynamic_gas/rex5_salt/sstore_100"`
/// or `"empty_transaction/revm_pinned"` when workload is empty. Used as the
/// stable key in `raw.json` / `state.json`'s rolling-median map.
pub fn row_key(group: &str, subject: &str, workload: &str) -> String {
    if workload.is_empty() {
        format!("{group}/{subject}")
    } else {
        format!("{group}/{subject}/{workload}")
    }
}

/// Locates the `target/criterion` directory relative to a repo checkout root.
pub fn criterion_dir_for(repo_checkout: &Path) -> PathBuf {
    repo_checkout.join("target").join("criterion")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_row(
        root: &Path,
        group: &str,
        dir_name: &str,
        function_id: &str,
        mean_ns: f64,
        std_dev_ns: f64,
        samples: &[(f64, f64)],
    ) {
        let new_dir = root.join(group).join(dir_name).join("new");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(
            new_dir.join("benchmark.json"),
            serde_json::json!({
                "group_id": group,
                "function_id": function_id,
                "full_id": format!("{group}/{function_id}"),
                "directory_name": format!("{group}/{dir_name}"),
                "title": format!("{group}/{function_id}"),
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            new_dir.join("estimates.json"),
            serde_json::json!({
                "mean": {"point_estimate": mean_ns, "confidence_interval": {}},
                "std_dev": {"point_estimate": std_dev_ns, "confidence_interval": {}},
            })
            .to_string(),
        )
        .unwrap();
        let iters: Vec<f64> = samples.iter().map(|(i, _)| *i).collect();
        let times: Vec<f64> = samples.iter().map(|(_, t)| *t).collect();
        fs::write(
            new_dir.join("sample.json"),
            serde_json::json!({"sampling_mode": "Auto", "iters": iters, "times": times})
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn test_scan_and_split_function_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_row(
            tmp.path(),
            "salt_dynamic_gas",
            "revm_pinned_sstore_100",
            "revm_pinned/sstore_100",
            14000.0,
            250.0,
            &[(100.0, 1_400_000.0)],
        );
        write_row(
            tmp.path(),
            "salt_dynamic_gas",
            "rex5_salt_sstore_100",
            "rex5_salt/sstore_100",
            28000.0,
            240.0,
            &[(100.0, 2_800_000.0)],
        );
        let rows = scan(tmp.path()).unwrap();
        assert_eq!(rows.len(), 2);
        let baseline = rows.iter().find(|r| r.subject == "revm_pinned").unwrap();
        assert_eq!(baseline.group, "salt_dynamic_gas");
        assert_eq!(baseline.workload, "sstore_100");
        assert_eq!(baseline.samples_ns, vec![14000.0]);
    }

    #[test]
    fn test_scan_skips_report_dir_and_incomplete_rows() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("report")).unwrap();
        fs::create_dir_all(tmp.path().join("some_group").join("report")).unwrap();
        // A row with only benchmark.json (e.g. an interrupted run) is skipped, not an error.
        let incomplete = tmp.path().join("some_group").join("incomplete_row").join("new");
        fs::create_dir_all(&incomplete).unwrap();
        fs::write(
            incomplete.join("benchmark.json"),
            serde_json::json!({"group_id": "some_group", "function_id": "x"}).to_string(),
        )
        .unwrap();
        let rows = scan(tmp.path()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_scan_value_parameterized_layout_one_level_deeper() {
        let tmp = tempfile::tempdir().unwrap();
        // `<group>/<function>/<value>/new` with value_str set, as criterion
        // lays out `BenchmarkId::new(function, value)` benches.
        let new_dir = tmp.path().join("param_group").join("rex5_case").join("100").join("new");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(
            new_dir.join("benchmark.json"),
            serde_json::json!({
                "group_id": "param_group",
                "function_id": "rex5_case",
                "value_str": "100",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            new_dir.join("estimates.json"),
            serde_json::json!({
                "mean": {"point_estimate": 5000.0},
                "std_dev": {"point_estimate": 50.0},
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            new_dir.join("sample.json"),
            serde_json::json!({"sampling_mode": "Auto", "iters": [1.0], "times": [5000.0]})
                .to_string(),
        )
        .unwrap();

        let rows = scan(tmp.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject, "rex5_case");
        assert_eq!(rows[0].workload, "100");
    }

    #[test]
    fn test_compute_ratios_against_revm_pinned() {
        let rows = vec![
            Row {
                group: "salt_dynamic_gas".into(),
                subject: "revm_pinned".into(),
                workload: "sstore_100".into(),
                mean_ns: 14000.0,
                std_dev_ns: 253.0,
                samples_ns: vec![14000.0],
            },
            Row {
                group: "salt_dynamic_gas".into(),
                subject: "rex5".into(),
                workload: "sstore_100".into(),
                mean_ns: 24000.0,
                std_dev_ns: 242.0,
                samples_ns: vec![24000.0],
            },
            Row {
                group: "salt_dynamic_gas".into(),
                subject: "rex5_salt".into(),
                workload: "sstore_100".into(),
                mean_ns: 28000.0,
                std_dev_ns: 248.0,
                samples_ns: vec![28000.0],
            },
        ];
        let ratios = compute_ratios(&rows, "revm_pinned");
        assert_eq!(ratios.len(), 1);
        let wl = &ratios[0];
        assert_eq!(wl.group, "salt_dynamic_gas");
        assert_eq!(wl.workload, "sstore_100");
        assert_eq!(wl.rows.len(), 3);
        let rex5_salt = wl.rows.iter().find(|r| r.subject == "rex5_salt").unwrap();
        assert!((rex5_salt.ratio_vs_baseline.unwrap() - 2.0).abs() < 1e-9);
        let baseline = wl.rows.iter().find(|r| r.subject == "revm_pinned").unwrap();
        assert!((baseline.ratio_vs_baseline.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_compute_ratios_missing_baseline_skips_ratio_not_the_group() {
        // A group/workload without a baseline row still shows up (mean_ns),
        // just with ratio_vs_baseline = None.
        let rows = vec![Row {
            group: "oracle_real_data".into(),
            subject: "rex5_oracle".into(),
            workload: "oracle_sload_50".into(),
            mean_ns: 5000.0,
            std_dev_ns: 100.0,
            samples_ns: vec![5000.0],
        }];
        let ratios = compute_ratios(&rows, "revm_pinned");
        assert_eq!(ratios.len(), 1);
        assert_eq!(ratios[0].rows[0].ratio_vs_baseline, None);
    }

    #[test]
    fn test_scan_bare_bench_function_top_level_layout() {
        // `Criterion::bench_function("name", ...)` writes directly at
        // `target/criterion/<name>/new` with a null function_id.
        let tmp = tempfile::tempdir().unwrap();
        let new_dir = tmp.path().join("standalone_bench").join("new");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(
            new_dir.join("benchmark.json"),
            serde_json::json!({"group_id": "standalone_bench", "function_id": null}).to_string(),
        )
        .unwrap();
        fs::write(
            new_dir.join("estimates.json"),
            serde_json::json!({
                "mean": {"point_estimate": 7000.0},
                "std_dev": {"point_estimate": 70.0},
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            new_dir.join("sample.json"),
            serde_json::json!({"sampling_mode": "Auto", "iters": [1.0], "times": [7000.0]})
                .to_string(),
        )
        .unwrap();

        let rows = scan(tmp.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group, "standalone_bench");
        assert_eq!(rows[0].subject, "standalone_bench");
        assert_eq!(rows[0].workload, "");
    }

    #[test]
    fn test_scan_coexisting_flat_and_value_layouts() {
        // The same function id can have both `<g>/<f>/new` and
        // `<g>/<f>/<value>/new`; neither may shadow the other.
        let tmp = tempfile::tempdir().unwrap();
        write_row(tmp.path(), "g", "f", "f", 1000.0, 10.0, &[(1.0, 1000.0)]);
        let nested = tmp.path().join("g").join("f").join("8").join("new");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("benchmark.json"),
            serde_json::json!({"group_id": "g", "function_id": "f", "value_str": "8"}).to_string(),
        )
        .unwrap();
        fs::write(
            nested.join("estimates.json"),
            serde_json::json!({
                "mean": {"point_estimate": 2000.0},
                "std_dev": {"point_estimate": 20.0},
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            nested.join("sample.json"),
            serde_json::json!({"sampling_mode": "Auto", "iters": [1.0], "times": [2000.0]})
                .to_string(),
        )
        .unwrap();

        let mut rows = scan(tmp.path()).unwrap();
        rows.sort_by(|a, b| a.workload.cmp(&b.workload));
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].subject.as_str(), rows[0].workload.as_str()), ("f", ""));
        assert_eq!((rows[1].subject.as_str(), rows[1].workload.as_str()), ("f", "8"));
    }

    #[test]
    fn test_unparseable_row_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        write_row(tmp.path(), "g", "good", "good", 1000.0, 10.0, &[(1.0, 1000.0)]);
        let bad = tmp.path().join("g").join("bad").join("new");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("benchmark.json"), "{not json").unwrap();
        fs::write(bad.join("estimates.json"), "{}").unwrap();
        fs::write(bad.join("sample.json"), "{}").unwrap();

        let rows = scan(tmp.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject, "good");
    }

    #[test]
    fn test_degenerate_baseline_yields_no_ratio_not_inf() {
        let rows = vec![
            Row {
                group: "g".into(),
                subject: "revm_pinned".into(),
                workload: "w".into(),
                mean_ns: 0.0,
                std_dev_ns: 0.0,
                samples_ns: vec![0.0],
            },
            Row {
                group: "g".into(),
                subject: "rex5".into(),
                workload: "w".into(),
                mean_ns: 1000.0,
                std_dev_ns: 10.0,
                samples_ns: vec![1000.0],
            },
        ];
        let ratios = compute_ratios(&rows, "revm_pinned");
        for row in &ratios[0].rows {
            assert_eq!(row.ratio_vs_baseline, None, "subject {}", row.subject);
        }
    }

    #[test]
    fn test_row_key_formats() {
        assert_eq!(
            row_key("salt_dynamic_gas", "rex5_salt", "sstore_100"),
            "salt_dynamic_gas/rex5_salt/sstore_100"
        );
        assert_eq!(
            row_key("empty_transaction", "revm_pinned", ""),
            "empty_transaction/revm_pinned"
        );
    }
}
