//! CLI-level checks for the `measure` subcommand: arg validation and help
//! text. No cargo bench / codspeed — those are covered by unit tests against
//! fixtures and the library API.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-bench-reporter"))
}

#[test]
fn test_measure_requires_a_lane_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let out = bin()
        .args([
            "measure",
            "--checkout",
            tmp.path().to_str().unwrap(),
            "--package",
            "pkg",
            "--bench-target",
            "t",
        ])
        .output()
        .expect("spawn measure");
    assert!(!out.status.success(), "expected nonzero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--instructions") && stderr.contains("--walltime"), "stderr: {stderr}");
    // stdout must stay empty on error (no partial JSON).
    assert!(out.stdout.is_empty(), "stdout: {}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn test_measure_rejects_missing_checkout() {
    let out = bin()
        .args([
            "measure",
            "--checkout",
            "/nonexistent-measure-cli-checkout",
            "--package",
            "pkg",
            "--bench-target",
            "t",
            "--walltime",
        ])
        .output()
        .expect("spawn measure");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a directory"), "stderr: {stderr}");
    assert!(out.stdout.is_empty());
}

#[test]
fn test_measure_help_mentions_filter_and_lanes() {
    let out = bin().args(["measure", "--help"]).output().expect("spawn help");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--instructions"), "{text}");
    assert!(text.contains("--walltime"), "{text}");
    assert!(text.contains("--bench-filter"), "{text}");
    assert!(text.contains("--checkout"), "{text}");
}
