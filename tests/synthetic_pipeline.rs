//! Synthetic dry run: drives the post-bench stage
//! (`pipeline::process_results`) with fixture criterion trees — no git, no
//! cargo, no benches — and asserts the storage layout, the simulated
//! regression/recovery events, and the 10-commit digest.

use mega_bench_reporter::config::Config;
use mega_bench_reporter::git::CommitMeta;
use mega_bench_reporter::instructions::{CollectOutcome, InstrCollection, InstrRow};
use mega_bench_reporter::pipeline::{process_results, Event, InstrVerdict};
use mega_bench_reporter::state::State;
use mega_bench_reporter::storage::{CommitRecord, RepoStore};
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
    // No instructions lane this run → no instr_bars.png.
    assert!(!first_dir.join("instr_bars.png").exists());
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
        Event::Regression { row_key, baseline_median, current, pct_over, metric, instructions } => {
            assert_eq!(row_key, "salt_dynamic_gas/rex5_salt/sstore_100");
            assert!((baseline_median - 2.0).abs() < 1e-9);
            assert!((current - 2.3).abs() < 1e-9);
            assert!((pct_over - 15.0).abs() < 0.1);
            // Walltime events carry no metric marker (absent in the JSON).
            assert_eq!(*metric, None);
            // This run has no instructions lane, so the walltime alert is
            // annotated `missing` (no comparable instructions data).
            let ann = instructions.as_ref().expect("walltime events carry the annotation");
            assert_eq!(ann.verdict, InstrVerdict::Missing);
            assert_eq!(ann.ratio_delta_pct, None);
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
            Some(CollectOutcome::Collected(instr_collection(salt_count))),
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
    // The instructions bars ride alongside compare_bars.png.
    assert!(first_dir.join("compare_bars.png").is_file());
    assert!(first_dir.join("instr_bars.png").is_file());
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
    let outcome = process_results(
        repo,
        &settings,
        &store,
        &scratch,
        &meta(0),
        vec![],
        Some(CollectOutcome::Collected(instr)),
    )
    .unwrap();

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
fn test_rerunning_same_sha_with_instr_does_not_double_count() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");

    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let run = || {
        process_results(
            repo,
            &settings,
            &store,
            &scratch,
            &meta(0),
            vec![],
            Some(CollectOutcome::Collected(instr_collection(20_000))),
        )
        .unwrap()
    };
    run();
    let state_after_first = State::load(&store.state_path()).unwrap();

    // Same sha again WITH instructions data: artifacts refresh, but neither
    // lane's rolling window nor the digest counter moves, and the replayed
    // ratios fire no events — mirroring the walltime-only rerun above.
    let outcome = run();
    assert!(outcome.events.is_empty());
    let state_after_rerun = State::load(&store.state_path()).unwrap();
    assert_eq!(state_after_first, state_after_rerun);
    let key = "salt_dynamic_gas/rex5_salt/sstore_100";
    assert_eq!(state_after_rerun.rows[key].recent_ratios.len(), 1);
    assert_eq!(state_after_rerun.instr_rows[key].recent_ratios.len(), 1);
    assert_eq!(state_after_rerun.commits_since_digest, 1);
}

#[test]
fn test_walltime_regression_annotated_with_instructions_verdict() {
    // A walltime regression fires at run 5 while the instructions lane sits at
    // a chosen level. The event's `instructions` annotation is the
    // instructions lane's *stateless* cross-check: its pre-update rolling
    // median (built over runs 0–4), its regression threshold (2%), symmetric
    // in both directions. Each verdict runs in its own fresh store.
    let verdict_for = |run5_instr: Option<InstrCollection>| -> (InstrVerdict, Option<f64>) {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = tmp.path().join("data");
        let cfg = Config::parse(CONFIG).unwrap();
        let repo = cfg.repo("mega-evm").unwrap();
        let settings = cfg.settings(repo).unwrap();
        let store = RepoStore::new(&data_root, "mega-evm");
        let scratch = tmp.path().join("criterion");

        let run = |i: usize, salt_ratio: f64, instr: Option<InstrCollection>| {
            if scratch.exists() {
                std::fs::remove_dir_all(&scratch).unwrap();
            }
            write_criterion_tree(&scratch, salt_ratio);
            let instr = instr.map(CollectOutcome::Collected);
            process_results(repo, &settings, &store, &scratch, &meta(i), vec![], instr).unwrap()
        };

        // Runs 0–4: walltime and instructions both flat (instr salt ratio 2.0
        // → the instructions rolling median settles at 2.0).
        for i in 0..5 {
            let out = run(i, 2.0, Some(instr_collection(20_000)));
            assert!(out.events.is_empty(), "run {i} should be quiet");
        }
        // Run 5: walltime jumps 15% (fires the walltime regression); the
        // instructions lane sits wherever `run5_instr` puts it.
        let out = run(5, 2.3, run5_instr);
        let ev = out
            .events
            .iter()
            .find(|e| matches!(e, Event::Regression { metric: None, .. }))
            .expect("a walltime regression event");
        match ev {
            Event::Regression { instructions, .. } => {
                let ann = instructions.as_ref().expect("walltime events carry the annotation");
                (ann.verdict, ann.ratio_delta_pct)
            }
            _ => unreachable!(),
        }
    };

    // flat: instructions unchanged (ratio 2.0 vs median 2.0 → 0%).
    assert_eq!(verdict_for(Some(instr_collection(20_000))), (InstrVerdict::Flat, Some(0.0)));
    // up: +3% (ratio 2.06 vs 2.0), at/above the 2% instructions threshold —
    // the walltime regression is corroborated by a real code-path change.
    assert_eq!(verdict_for(Some(instr_collection(20_600))), (InstrVerdict::Up, Some(3.0)));
    // down: −3% (ratio 1.94 vs 2.0), symmetric — an instructions improvement.
    assert_eq!(verdict_for(Some(instr_collection(19_400))), (InstrVerdict::Down, Some(-3.0)));
    // missing: no instructions data at all this run.
    assert_eq!(verdict_for(None), (InstrVerdict::Missing, None));
}

#[test]
fn test_digest_over_mixed_instr_commits_has_instr_series_and_chart() {
    // 10 commits with instructions data on even runs only. The 10th commit's
    // digest must null-pad `instr_series` at the no-instr commits and render
    // `instr_trend.png`. Walltime stays flat, so the digest is the only event.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();
    let store = RepoStore::new(&data_root, "mega-evm");
    let scratch = tmp.path().join("criterion");

    let mut digest_rel_dir = None;
    for i in 0..10 {
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch).unwrap();
        }
        write_criterion_tree(&scratch, 2.0);
        let instr = (i % 2 == 0).then(|| CollectOutcome::Collected(instr_collection(20_000)));
        let out =
            process_results(repo, &settings, &store, &scratch, &meta(i), vec![], instr).unwrap();
        for ev in &out.events {
            if let Event::Digest { dir } = ev {
                digest_rel_dir = Some(dir.clone());
            }
        }
    }

    let digest_dir = store.root().join(digest_rel_dir.expect("digest fired at the 10th commit"));
    assert!(digest_dir.join("trend.png").is_file());
    assert!(digest_dir.join("instr_trend.png").is_file());

    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(digest_dir.join("summary.json")).unwrap())
            .unwrap();
    let instr_series = summary["instr_series"].as_array().expect("instr_series present");
    let salt = instr_series
        .iter()
        .find(|r| r["row_key"] == "salt_dynamic_gas/rex5_salt/sstore_100")
        .expect("salt headline row in instr_series");
    let ratios = salt["ratios"].as_array().unwrap();
    assert_eq!(ratios.len(), 10, "aligned to the same 10 commits as the walltime series");
    for (i, ratio) in ratios.iter().enumerate() {
        if i % 2 == 0 {
            assert!(ratio.is_number(), "commit {i} has instructions data");
        } else {
            assert!(ratio.is_null(), "commit {i} has no instructions data → null");
        }
    }
}

#[test]
fn test_trend_metric_instructions_happy_and_empty() {
    use mega_bench_reporter::digest::{self, TrendRequest};
    use mega_bench_reporter::lane::Lane;

    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();

    // Runs `n` commits into a fresh store, `with_instr` toggling the lane, and
    // returns (store, window) ready to hand to build_adhoc_trend.
    let stored = |with_instr: bool| {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
        let scratch = tmp.path().join("criterion");
        for i in 0..3 {
            if scratch.exists() {
                std::fs::remove_dir_all(&scratch).unwrap();
            }
            write_criterion_tree(&scratch, 2.0);
            let instr = with_instr.then(|| CollectOutcome::Collected(instr_collection(20_000)));
            process_results(repo, &settings, &store, &scratch, &meta(i), vec![], instr).unwrap();
        }
        let records: Vec<_> =
            store.load_commit_records().unwrap().into_iter().map(|(_, r)| r).collect();
        let window = digest::select_window(records, 20, None, None).unwrap();
        // The tempdir must outlive the trend build that reads/writes under it.
        (tmp, store, window)
    };

    let trend = |store: &RepoStore, window: &[CommitRecord], metric: Lane| {
        digest::build_adhoc_trend(
            store,
            "mega-evm",
            &repo.headline_label(),
            |s| repo.is_headline(s),
            settings.regression_threshold_pct,
            settings.instr_regression_threshold_pct,
            metric,
            window,
            TrendRequest { row_patterns: &[], out: None },
        )
    };

    // Happy path: instructions data present → instr_trend.png plus a
    // summary.json carrying instr_series; only the chosen lane's chart lands.
    let (_tmp, store, window) = stored(true);
    let ok = trend(&store, &window, Lane::Instructions).unwrap();
    assert!(ok.dir.join("instr_trend.png").is_file());
    assert!(!ok.dir.join("trend.png").exists());
    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(ok.dir.join("summary.json")).unwrap())
            .unwrap();
    assert!(summary["instr_series"].is_array());

    // Empty window: commits with no instructions data → an actionable error.
    let (_tmp, store, window) = stored(false);
    let err = trend(&store, &window, Lane::Instructions);
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("instructions"));
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

#[test]
fn test_require_instructions_skip_or_failure_errors_after_full_writes() {
    // `require_instructions = true` turns a skipped lane — or a lane-failed
    // target — into a nonzero exit, but only AFTER the complete happy-path
    // write sequence: everything on disk must be exactly what a best-effort
    // run would have written (README invariant: "state is written last,
    // nothing half-applied"); the error is a signal, not a data problem.
    let cfg =
        Config::parse(&format!("{CONFIG}\n[repos.instructions]\nrequire_instructions = true\n"))
            .unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();

    // Case 1: the lane skipped entirely (the preflight seam's tools-missing
    // outcome); the captured reason travels into the error message.
    let tmp = tempfile::tempdir().unwrap();
    let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let err = process_results(
        repo,
        &settings,
        &store,
        &scratch,
        &meta(0),
        vec![],
        Some(CollectOutcome::Skipped("codspeed CLI not usable: spawn failed".into())),
    )
    .expect_err("require_instructions + skipped lane must fail the run");
    let msg = format!("{err:#}");
    assert!(msg.contains("require_instructions"), "error names the knob: {msg}");
    assert!(msg.contains("codspeed CLI not usable"), "error names the skip reason: {msg}");

    // The walltime write sequence finished before the error fired.
    let commit_dir = store.root().join("commits").join(format!("20260701-{}", &meta(0).sha[..7]));
    let raw_text = std::fs::read_to_string(commit_dir.join("raw.json")).unwrap();
    assert!(!raw_text.contains("\"instr\""), "skipped lane leaves no instr blocks");
    assert!(commit_dir.join("compare_bars.png").is_file());
    assert!(commit_dir.join("compare_table.json").is_file());
    let events: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(commit_dir.join("events.json")).unwrap())
            .unwrap();
    assert_eq!(events, serde_json::json!([]), "events.json written (empty first run)");
    let state = State::load(&store.state_path()).unwrap();
    assert_eq!(state.last_seen_sha.as_deref(), Some(meta(0).sha.as_str()), "state saved");
    assert!(state.rows.contains_key("salt_dynamic_gas/rex5_salt/sstore_100"));
    let latest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(store.root().join("latest.json")).unwrap())
            .unwrap();
    assert_eq!(latest["sha"], meta(0).sha.as_str(), "latest.json updated");

    // Case 2: the lane ran but a bench target failed collection — the error
    // names the target, and the successful rows' data still landed first.
    let tmp = tempfile::tempdir().unwrap();
    let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let mut instr = instr_collection(20_000);
    instr.failed_targets = vec!["mega_bench".to_string()];
    let err = process_results(
        repo,
        &settings,
        &store,
        &scratch,
        &meta(0),
        vec![],
        Some(CollectOutcome::Collected(instr)),
    )
    .expect_err("require_instructions + lane-failed target must fail the run");
    let msg = format!("{err:#}");
    assert!(msg.contains("require_instructions"), "error names the knob: {msg}");
    assert!(msg.contains("mega_bench"), "error names the failed target: {msg}");
    let commit_dir = store.root().join("commits").join(format!("20260701-{}", &meta(0).sha[..7]));
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(commit_dir.join("raw.json")).unwrap())
            .unwrap();
    assert_eq!(raw["instr_failed_targets"][0], "mega_bench");
    assert_eq!(raw["groups"]["salt_dynamic_gas"]["rex5_salt/sstore_100"]["instr"]["count"], 20_000);
    let state = State::load(&store.state_path()).unwrap();
    assert_eq!(state.last_seen_sha.as_deref(), Some(meta(0).sha.as_str()));
    assert!(state.instr_rows.contains_key("salt_dynamic_gas/rex5_salt/sstore_100"));
}

#[test]
fn test_require_instructions_absent_or_false_skip_stays_best_effort() {
    // With the knob absent or explicitly false, a skipped lane keeps today's
    // behavior exactly: Ok, quiet, artifacts byte-identical to a
    // walltime-only run with no `[repos.instructions]` at all.
    let run_into = |config: &str, instr: Option<CollectOutcome>| -> (String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::parse(config).unwrap();
        let repo = cfg.repo("mega-evm").unwrap();
        let settings = cfg.settings(repo).unwrap();
        let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
        let scratch = tmp.path().join("criterion");
        write_criterion_tree(&scratch, 2.0);
        let outcome =
            process_results(repo, &settings, &store, &scratch, &meta(0), vec![], instr).unwrap();
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.instr_failed_targets, None);
        (
            std::fs::read_to_string(outcome.commit_dir.join("raw.json")).unwrap(),
            std::fs::read_to_string(outcome.commit_dir.join("compare_table.json")).unwrap(),
        )
    };

    // Today's baseline: lane off entirely.
    let (control_raw, control_table) = run_into(CONFIG, None);
    for knob in ["", "require_instructions = false\n"] {
        let config = format!("{CONFIG}\n[repos.instructions]\n{knob}");
        let skipped = Some(CollectOutcome::Skipped(
            "skipped on macos (CodSpeed simulation mode needs Linux/valgrind)".into(),
        ));
        let (raw, table) = run_into(&config, skipped);
        assert_eq!(raw, control_raw, "raw.json byte-identical (knob: {knob:?})");
        assert_eq!(table, control_table, "compare_table.json byte-identical (knob: {knob:?})");
    }
}

#[test]
fn test_skip_bench_regen_carries_instr_blocks_byte_identical() {
    // The --skip-bench carry-forward: a re-render reads the previous raw.json
    // for the sha, reconstructs the lane collection from its rows, and
    // re-attaches it — instr blocks (and the compare-table columns derived
    // from them) come out byte-identical. A previous record without instr
    // data regenerates exactly as today.
    let cfg = Config::parse(CONFIG).unwrap();
    let repo = cfg.repo("mega-evm").unwrap();
    let settings = cfg.settings(repo).unwrap();

    // Locator for the previous record — the same probe record the pipeline's
    // --skip-bench branch builds from the commit meta.
    let probe = || {
        CommitRecord::new(
            meta(0).sha.clone(),
            meta(0).date.clone(),
            meta(0).rustc.clone(),
            "revm_pinned".to_string(),
        )
    };

    // Original run WITH instructions data.
    let tmp = tempfile::tempdir().unwrap();
    let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let out = process_results(
        repo,
        &settings,
        &store,
        &scratch,
        &meta(0),
        vec![],
        Some(CollectOutcome::Collected(instr_collection(20_000))),
    )
    .unwrap();
    let raw_path = out.commit_dir.join("raw.json");
    let table_path = out.commit_dir.join("compare_table.json");
    let original_raw = std::fs::read_to_string(&raw_path).unwrap();
    let original_table = std::fs::read_to_string(&table_path).unwrap();
    assert!(original_raw.contains("\"instr\""), "sanity: the original record carries instr data");

    // What the --skip-bench branch does: read the previous record, carry its
    // instr collection forward, re-render under the same sha.
    let previous = store
        .load_commit_record(&probe())
        .unwrap()
        .expect("previous raw.json for the last processed sha");
    let carried = previous.instr_collection().expect("previous record carries instr data");
    let regen = process_results(
        repo,
        &settings,
        &store,
        &scratch,
        &meta(0),
        vec![],
        Some(CollectOutcome::Collected(carried)),
    )
    .unwrap();
    assert!(regen.events.is_empty(), "regen of the same sha is a rerun: no events");
    assert_eq!(
        std::fs::read_to_string(&raw_path).unwrap(),
        original_raw,
        "raw.json (instr blocks included) byte-identical after the carry-forward regen"
    );
    assert_eq!(
        std::fs::read_to_string(&table_path).unwrap(),
        original_table,
        "compare_table.json instr columns byte-identical after the carry-forward regen"
    );

    // A previous raw.json WITHOUT instr data → nothing to carry → the regen
    // is unchanged from today.
    let tmp = tempfile::tempdir().unwrap();
    let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
    let scratch = tmp.path().join("criterion");
    write_criterion_tree(&scratch, 2.0);
    let out = process_results(repo, &settings, &store, &scratch, &meta(0), vec![], None).unwrap();
    let raw_path = out.commit_dir.join("raw.json");
    let original_raw = std::fs::read_to_string(&raw_path).unwrap();
    let previous = store.load_commit_record(&probe()).unwrap().expect("previous raw.json");
    assert_eq!(previous.instr_collection(), None, "no instr data to carry");
    process_results(repo, &settings, &store, &scratch, &meta(0), vec![], None).unwrap();
    assert_eq!(std::fs::read_to_string(&raw_path).unwrap(), original_raw);

    // The helper's edges, as the pipeline consumes them: an absent previous
    // record is Ok(None) (regenerate as today); a malformed one is Err — the
    // pipeline warns (`instructions lane:` prefix) and regenerates without
    // instr data rather than failing the regen.
    let tmp = tempfile::tempdir().unwrap();
    let store = RepoStore::new(&tmp.path().join("data"), "mega-evm");
    assert!(store.load_commit_record(&probe()).unwrap().is_none(), "absent → Ok(None)");
    let dir = store.commits_dir().join(format!("20260701-{}", &meta(0).sha[..7]));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("raw.json"), "{not json").unwrap();
    assert!(store.load_commit_record(&probe()).is_err(), "malformed → Err, caller regenerates");
}
