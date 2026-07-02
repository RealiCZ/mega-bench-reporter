//! Task 1.7's synthetic dry run: drives the post-bench stage
//! (`pipeline::process_results`) with fixture criterion trees — no git, no
//! cargo, no benches — and asserts the storage layout, the simulated
//! regression/recovery alert cards, and the 10-commit digest card.

use mega_bench_reporter::cards::CardKind;
use mega_bench_reporter::config::Config;
use mega_bench_reporter::pipeline::{process_results, CommitMeta};
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
headline_spec = "rex5"
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
        process_results(repo, &settings, &store, &scratch, &meta(i), vec![]).unwrap()
    };

    // Runs 0–4: stable baseline. No cards at all.
    for i in 0..5 {
        let outcome = run(i, 2.0);
        assert!(
            outcome.cards.is_empty(),
            "run {i} should be quiet, got {:?}",
            outcome.cards.iter().map(|c| c.kind).collect::<Vec<_>>()
        );
    }

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
    assert_eq!(table["headline_label"], "rex5");
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
    assert_eq!(
        raw["groups"]["salt_dynamic_gas"]["rex5_salt/sstore_100"]["ratio_vs_revm_pinned"],
        2.0
    );
    assert_eq!(
        raw["groups"]["oracle_real_data"]["rex5_oracle/oracle_sload_50"]["ratio_vs_revm_pinned"],
        serde_json::Value::Null
    );

    // Run 5: the headline row jumps 15% over the rolling median → alert card.
    let outcome = run(5, 2.3);
    assert_eq!(outcome.cards.len(), 1);
    let alert = &outcome.cards[0];
    assert_eq!(alert.kind, CardKind::RegressionAlert);
    let text = serde_json::to_string(&alert.card).unwrap();
    assert!(text.contains("salt_dynamic_gas/rex5_salt/sstore_100"));
    assert!(text.contains("+15.0%"));
    for attachment in &alert.attachments {
        assert!(attachment.is_file(), "attachment missing: {}", attachment.display());
    }

    // The cards are persisted durably in the commit dir (recovery path for a
    // lost stdout), and a retry of the same sha must NOT clobber them with
    // its empty card list.
    let alert_dir = store.root().join("commits").join(format!("20260702-{}", &meta(5).sha[..7]));
    let read_persisted = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(alert_dir.join("cards.json")).unwrap())
            .unwrap()
    };
    assert_eq!(read_persisted()["cards"][0]["kind"], "regression_alert");
    let retry = run(5, 2.3);
    assert!(retry.cards.is_empty(), "idempotent rerun emits no cards on stdout");
    assert_eq!(
        read_persisted()["cards"][0]["kind"],
        "regression_alert",
        "rerun must not overwrite the persisted cards"
    );

    // Run 6: still elevated — no re-alert (latched).
    let outcome = run(6, 2.32);
    assert!(outcome.cards.is_empty(), "still-regressed must not re-alert");

    // Run 7: back to baseline → recovery card.
    let outcome = run(7, 2.0);
    assert_eq!(outcome.cards.len(), 1);
    assert_eq!(outcome.cards[0].kind, CardKind::Recovery);

    // Runs 8–9: quiet again; run 9 is the 10th commit → trend digest card.
    let outcome = run(8, 2.0);
    assert!(outcome.cards.is_empty());
    let outcome = run(9, 2.0);
    assert_eq!(outcome.cards.len(), 1);
    let digest = &outcome.cards[0];
    assert_eq!(digest.kind, CardKind::TrendDigest);
    let digest_text = serde_json::to_string(&digest.card).unwrap();
    assert!(digest_text.contains("10"));
    assert!(digest_text.contains("${image:trend.png}"));

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
    process_results(repo, &settings, &store, &scratch, &meta(0), vec![]).unwrap();
    let state_after_first = State::load(&store.state_path()).unwrap();

    // Same sha again (e.g. a relaying-agent retry): artifacts refresh, but the
    // rolling window and digest counter must not move.
    let outcome = process_results(repo, &settings, &store, &scratch, &meta(0), vec![]).unwrap();
    assert!(outcome.cards.is_empty());
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
    let outcome =
        process_results(repo, &settings, &store, &scratch, &meta(0), vec!["block_bench".into()])
            .unwrap();

    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(outcome.commit_dir.join("raw.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["failed_targets"][0], "block_bench");
}
