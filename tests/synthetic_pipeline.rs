//! Synthetic dry run: drives the post-bench stage
//! (`pipeline::process_results`) with fixture criterion trees — no git, no
//! cargo, no benches — and asserts the storage layout, the simulated
//! regression/recovery events, and the 10-commit digest.

use mega_bench_reporter::config::Config;
use mega_bench_reporter::git::CommitMeta;
use mega_bench_reporter::instructions::{InstrCollection, InstrRow};
use mega_bench_reporter::pipeline::{process_results, Event};
use mega_bench_reporter::state::State;
use mega_bench_reporter::storage::RepoStore;
use std::path::Path;

const CONFIG: &str = r#"
[[repos]]
name = "mega-evm"
github = "megaeth-labs/mega-evm"
branch = "main"
clone_url = "https://github.com/megaeth-labs/mega-evm.git"
bench_targets = ["mega_bench"]
baseline_subject = "revm_pinned"
headline_subjects = ["rex5", "rex5_*"]
"#;

/// Writes one criterion benchmark dir (`<group>/<dir>/new/*.json`).
fn write_bench(root: &Path, group: &str, function_id: &str, mean_ns: f64) {
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
    // 20 deterministic samples around the mean.
    let samples: Vec<f64> =
        (0..20).map(|i| mean_ns * (1.0 + 0.01 * ((i as f64) * 2.399).sin())).collect();
    std::fs::write(
        new_dir.join("sample.json"),
        serde_json::json!({
            "sampling_mode": "Auto",
            "iters": vec![1.0; 20],
            "times": samples,
        })
        .to_string(),
    )
    .unwrap();
}

/// A full fixture criterion tree for one synthetic commit. `salt_ratio`
/// controls the headline row's gap (baseline stays fixed), so a run can
/// simulate a regression by passing a higher value.
fn write_criterion_tree(root: &Path, salt_ratio: f64) {
    write_bench(root, "salt_dynamic_gas", "revm_pinned/sstore_100", 14_000.0);
    write_bench(root, "salt_dynamic_gas", "rex5/sstore_100", 24_000.0);
    write_bench(root, "salt_dynamic_gas", "rex5_salt/sstore_100", 14_000.0 * salt_ratio);
    write_bench(root, "empty_transaction", "revm_pinned", 8_000.0);
    write_bench(root, "empty_transaction", "rex5", 9_500.0);
    // A group without a revm_pinned baseline: rows recorded, no ratio.
    write_bench(root, "oracle_real_data", "rex5_oracle/oracle_sload_50", 5_000.0);
}

fn meta(i: usize) -> CommitMeta {
    // Distinct leading 7 chars per synthetic sha — commit dirs are keyed by
    // `<day>-<shortsha>`.
    CommitMeta {
        sha: format!("{:07x}{}", 0xa00000 + i, "0".repeat(33)),
        date: format!("2026-07-{:02}T{:02}:15:00Z", 1 + i / 4, 6 * (i % 4)),
        rustc: "rustc 1.86.0".to_string(),
    }
}

#[test]
fn test_synthetic_ten_commit_run_with_regression_and_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");

    let scratch = tmp.path().join("criterion");
    let run = |i: usize, salt_ratio: f64| {
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch).unwrap();
        }
        write_criterion_tree(&scratch, salt_ratio);
        process_results(repo, &settings, &store, &scratch, &meta(i), vec![], None).unwrap()
    };

    // Runs 0–4: stable baseline. No events at all.
    for i in 0..5 {
        let outcome = run(i, 2.0);
        assert!(outcome.events.is_empty(), "run {i} should be quiet, got {:?}", outcome.events);
    }

    // Discovery pointer follows the newest completed run.
    let latest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(store.root().join("latest.json")).unwrap())
            .unwrap();
    assert_eq!(latest["sha"], meta(4).sha.as_str());

    // Storage layout after the stable runs.
    let commit_dirs: Vec<_> = std::fs::read_dir(store.root().join("commits"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(commit_dirs.len(), 5, "one commits/ dir per run: {commit_dirs:?}");
    let first_dir = store.root().join("commits").join(format!("20260701-{}", &meta(0).sha[..7]));
    assert!(first_dir.join("raw.json").is_file());
    assert!(first_dir.join("compare_bars.png").is_file());
    let table: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(first_dir.join("compare_table.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(table["headline_label"], "rex5, rex5_*");
    assert!(table["subjects"].as_array().unwrap().iter().any(|s| s == "revm_pinned"));
    let salt_row = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item"] == "salt_dynamic_gas/sstore_100")
        .expect("salt row in compare_table.json");
    assert_eq!(salt_row["headline_ratio"], 2.0);
    assert!(first_dir.join("dist_salt_dynamic_gas_sstore_100.png").is_file());
    assert!(first_dir.join("dist_empty_transaction.png").is_file());
    // Baseline-less group gets no violin with a single row, and no ratio.
    assert!(!first_dir.join("dist_oracle_real_data_oracle_sload_50.png").exists());

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(first_dir.join("raw.json")).unwrap())
            .unwrap();
    assert_eq!(raw["baseline_subject"], "revm_pinned");
    assert_eq!(raw["groups"]["salt_dynamic_gas"]["rex5_salt/sstore_100"]["ratio_vs_baseline"], 2.0);
    assert_eq!(
        raw["groups"]["oracle_real_data"]["rex5_oracle/oracle_sload_50"]["ratio_vs_baseline"],
        serde_json::Value::Null
    );

    // Run 5: the headline row jumps 15% over the rolling median → regression
    // event (a fact — composing an alert card from it is the consumer's job).
    let outcome = run(5, 2.3);
    assert_eq!(outcome.events.len(), 1);
    match &outcome.events[0] {
        Event::Regression { row_key, baseline_median, current, pct_over, metric } => {
            assert_eq!(row_key, "salt_dynamic_gas/rex5_salt/sstore_100");
            assert!((baseline_median - 2.0).abs() < 1e-9);
            assert!((current - 2.3).abs() < 1e-9);
            assert!((pct_over - 15.0).abs() < 0.1);
            // Walltime events carry no metric marker (absent in the JSON).
            assert_eq!(*metric, None);
        }
        other => panic!("expected Regression, got {other:?}"),
    }

    // The events are persisted durably in the commit dir (recovery path for a
    // lost stdout), and a retry of the same sha must NOT clobber them with
    // its empty event list.
    let alert_dir = store.root().join("commits").join(format!("20260702-{}", &meta(5).sha[..7]));
    let read_persisted = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(alert_dir.join("events.json")).unwrap())
            .unwrap()
    };
    assert_eq!(read_persisted()[0]["type"], "regression");
    let retry = run(5, 2.3);
    assert!(retry.events.is_empty(), "idempotent rerun emits no events on stdout");
    assert_eq!(
        read_persisted()[0]["type"],
        "regression",
        "rerun must not overwrite the persisted events"
    );

    // Run 6: still elevated — no new event (latched).
    let outcome = run(6, 2.32);
    assert!(outcome.events.is_empty(), "still-regressed must not re-fire");

    // Run 7: back to baseline → recovery event.
    let outcome = run(7, 2.0);
    assert_eq!(outcome.events.len(), 1);
    assert!(matches!(outcome.events[0], Event::Recovery { .. }));

    // Runs 8–9: quiet again; run 9 is the 10th commit → digest event.
    let outcome = run(8, 2.0);
    assert!(outcome.events.is_empty());
    let outcome = run(9, 2.0);
    assert_eq!(outcome.events.len(), 1);
    match &outcome.events[0] {
        Event::Digest { dir } => assert!(dir.starts_with("digests/"), "digest dir: {dir}"),
        other => panic!("expected Digest, got {other:?}"),
    }

    // Digest artifacts on disk.
    let digest_dirs: Vec<_> = std::fs::read_dir(store.root().join("digests"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(digest_dirs.len(), 1);
    assert!(digest_dirs[0].join("summary.json").is_file());
    assert!(digest_dirs[0].join("trend.png").is_file());
    let summary: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(digest_dirs[0].join("summary.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["commits"].as_array().unwrap().len(), 10);
    // The regression bump at runs 5–6 is visible in the stored series.
    let salt_row = summary["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["row_key"] == "salt_dynamic_gas/rex5_salt/sstore_100")
        .expect("headline row in summary");
    assert_eq!(salt_row["ratios"][5], 2.3);

    // State after all runs: digest counter reset, sha recorded, latch clear.
    let state = State::load(&store.state_path()).unwrap();
    assert_eq!(state.commits_since_digest, 0);
    assert_eq!(state.last_seen_sha.as_deref(), Some(meta(9).sha.as_str()));
    assert!(!state.rows["salt_dynamic_gas/rex5_salt/sstore_100"].currently_regressed);
}

/// A synthetic instructions-lane collection mirroring `write_criterion_tree`'s
/// headline row; `salt_count` controls the `rex5_salt` count (baseline fixed
/// at 10_000, so `salt_count / 10_000` is the instructions ratio).
fn instr_collection(salt_count: u64) -> InstrCollection {
    let mk = |group: &str, subject: &str, workload: &str, count: u64| InstrRow {
        group: group.into(),
        subject: subject.into(),
        workload: workload.into(),
        count,
    };
    InstrCollection {
        rows: vec![
            mk("salt_dynamic_gas", "revm_pinned", "sstore_100", 10_000),
            mk("salt_dynamic_gas", "rex5", "sstore_100", 24_000),
            mk("salt_dynamic_gas", "rex5_salt", "sstore_100", salt_count),
            mk("empty_transaction", "revm_pinned", "", 8_000),
            mk("empty_transaction", "rex5", "", 9_500),
        ],
        failed_targets: vec![],
    }
}

#[test]
fn test_instructions_lane_regression_recovery_and_rebaseline() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");

    let scratch = tmp.path().join("criterion");
    let run = |i: usize, salt_count: u64| {
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch).unwrap();
        }
        // Walltime stays flat throughout: every event below is instructions-only.
        write_criterion_tree(&scratch, 2.0);
        process_results(
            repo,
            &settings,
            &store,
            &scratch,
            &meta(i),
            vec![],
            Some(instr_collection(salt_count)),
        )
        .unwrap()
    };

    // Runs 0–4: stable counts. Quiet on both lanes.
    for i in 0..5 {
        let outcome = run(i, 20_000);
        assert!(outcome.events.is_empty(), "run {i} should be quiet, got {:?}", outcome.events);
        assert_eq!(outcome.instr_failed_targets, None, "clean lane reports no failures");
    }

    // The per-commit artifacts carry the lane's data.
    let first_dir = store.root().join("commits").join(format!("20260701-{}", &meta(0).sha[..7]));
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(first_dir.join("raw.json")).unwrap())
            .unwrap();
    let salt_row = &raw["groups"]["salt_dynamic_gas"]["rex5_salt/sstore_100"];
    assert_eq!(salt_row["instr"]["count"], 20_000);
    assert_eq!(salt_row["instr"]["ratio_vs_baseline"], 2.0);
    // Lane on but all targets fine: no instr_failed_targets key.
    assert!(raw.get("instr_failed_targets").is_none());
    let table: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(first_dir.join("compare_table.json")).unwrap(),
    )
    .unwrap();
    let salt_item = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item"] == "salt_dynamic_gas/sstore_100")
        .expect("salt row in compare_table.json");
    // Worst (max) headline-family instructions ratio: plain rex5's
    // 24_000/10_000 beats rex5_salt's 2.0.
    assert_eq!(salt_item["instr_headline_ratio"], 2.4);
    let subjects = table["subjects"].as_array().unwrap();
    let instr_cols = salt_item["instr"].as_array().unwrap();
    assert_eq!(instr_cols.len(), subjects.len(), "instr aligns with subjects");
    let pinned_idx = subjects.iter().position(|s| s == "revm_pinned").unwrap();
    assert_eq!(instr_cols[pinned_idx], 10_000);

    // Run 5: counts jump 3% — over the 2% built-in instructions threshold,
    // far under the 10% walltime threshold. Exactly one event, marked.
    let outcome = run(5, 20_600);
    assert_eq!(outcome.events.len(), 1, "got {:?}", outcome.events);
    match &outcome.events[0] {
        Event::Regression { row_key, metric, pct_over, .. } => {
            assert_eq!(row_key, "salt_dynamic_gas/rex5_salt/sstore_100");
            assert_eq!(metric.as_deref(), Some("instructions"));
            assert!((pct_over - 3.0).abs() < 0.1);
        }
        other => panic!("expected instructions Regression, got {other:?}"),
    }
    // The persisted events carry the marker too.
    let alert_dir = store.root().join("commits").join(format!("20260702-{}", &meta(5).sha[..7]));
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(alert_dir.join("events.json")).unwrap())
            .unwrap();
    assert_eq!(persisted[0]["metric"], "instructions");

    // Run 6: still elevated — latched, quiet.
    assert!(run(6, 20_600).events.is_empty(), "still-regressed must not re-fire");

    // Run 7: back to the old count → recovery, still marked.
    let outcome = run(7, 20_000);
    assert_eq!(outcome.events.len(), 1);
    match &outcome.events[0] {
        Event::Recovery { metric, .. } => assert_eq!(metric.as_deref(), Some("instructions")),
        other => panic!("expected instructions Recovery, got {other:?}"),
    }

    // The lanes' histories live side by side under the same key.
    let state = State::load(&store.state_path()).unwrap();
    let key = "salt_dynamic_gas/rex5_salt/sstore_100";
    assert!(state.rows.contains_key(key));
    assert!(state.instr_rows.contains_key(key));
    assert!(!state.instr_rows[key].currently_regressed);

    // Rebaseline clears the row from BOTH lanes.
    let mut state = state;
    let cleared = state.clear_rows(&[format!("{key}*")]);
    assert_eq!(cleared, vec![key.to_string()]);
    assert!(!state.rows.contains_key(key));
    assert!(!state.instr_rows.contains_key(key));
}

#[test]
fn test_instr_failed_targets_are_marked_in_record_and_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");

    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let mut instr = instr_collection(20_000);
    instr.failed_targets = vec!["mega_bench".to_string()];
    let outcome =
        process_results(repo, &settings, &store, &scratch, &meta(0), vec![], Some(instr)).unwrap();

    assert_eq!(outcome.instr_failed_targets, Some(vec!["mega_bench".to_string()]));
    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(outcome.commit_dir.join("raw.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["instr_failed_targets"][0], "mega_bench");
    // The walltime marker stays independent (and absent here).
    assert!(raw.get("failed_targets").is_none());
}

#[test]
fn test_rerunning_same_sha_does_not_double_count() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");

    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    process_results(repo, &settings, &store, &scratch, &meta(0), vec![], None).unwrap();
    let state_after_first = State::load(&store.state_path()).unwrap();

    // Same sha again (e.g. a consumer retry): artifacts refresh, but the
    // rolling window and digest counter must not move.
    let outcome =
        process_results(repo, &settings, &store, &scratch, &meta(0), vec![], None).unwrap();
    assert!(outcome.events.is_empty());
    let state_after_rerun = State::load(&store.state_path()).unwrap();
    assert_eq!(state_after_first, state_after_rerun);
    assert_eq!(
        state_after_rerun.rows["salt_dynamic_gas/rex5_salt/sstore_100"].recent_ratios.len(),
        1
    );
    assert_eq!(state_after_rerun.commits_since_digest, 1);
}

#[test]
fn test_failed_targets_are_marked_not_silently_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");

    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let outcome = process_results(
        repo,
        &settings,
        &store,
        &scratch,
        &meta(0),
        vec!["block_bench".into()],
        None,
    )
    .unwrap();

    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(outcome.commit_dir.join("raw.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["failed_targets"][0], "block_bench");
}
