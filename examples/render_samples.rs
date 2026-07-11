//! Throwaway visual check: renders one of each chart type into OUT_DIR arg.
use mega_bench_reporter::charts::*;
use mega_bench_reporter::compare::build_compare_table;
use mega_bench_reporter::criterion_results::{RatioRow, Row, WorkloadRatios};
use std::path::Path;

fn main() {
    let out = std::env::args().nth(1).expect("usage: render_samples <out-dir>");
    let out = Path::new(&out);
    std::fs::create_dir_all(out).unwrap();

    let mk_row = |group: &str, subject: &str, workload: &str, center: f64| Row {
        group: group.into(),
        subject: subject.into(),
        workload: workload.into(),
        mean_ns: center,
        std_dev_ns: center * 0.015,
        samples_ns: (0..100)
            .map(|i| {
                center
                    * (1.0 + 0.015 * ((i as f64 * 2.399).sin()) + 0.01 * ((i as f64 * 0.71).cos()))
            })
            .collect(),
    };
    let ratio_row = |subject: &str, mean: f64, ratio: f64| RatioRow {
        subject: subject.into(),
        mean_ns: mean,
        ratio_vs_baseline: Some(ratio),
    };

    let rows = vec![
        mk_row("salt_dynamic_gas", "revm_pinned", "sstore_100", 13970.0),
        mk_row("salt_dynamic_gas", "rex4", "sstore_100", 20000.0),
        mk_row("salt_dynamic_gas", "rex5", "sstore_100", 24040.0),
        mk_row("salt_dynamic_gas", "rex5_salt", "sstore_100", 28870.0),
        mk_row("oracle_real_data", "revm_pinned", "oracle_sload_50", 5000.0),
        mk_row("oracle_real_data", "rex5_oracle", "oracle_sload_50", 3500.0),
    ];
    let ratios = vec![
        WorkloadRatios {
            group: "salt_dynamic_gas".into(),
            workload: "sstore_100".into(),
            rows: vec![
                ratio_row("revm_pinned", 13970.0, 1.0),
                ratio_row("rex4", 20000.0, 1.43),
                ratio_row("rex5", 24040.0, 1.72),
                ratio_row("rex5_salt", 28870.0, 2.07),
            ],
        },
        WorkloadRatios {
            group: "oracle_real_data".into(),
            workload: "oracle_sload_50".into(),
            rows: vec![
                ratio_row("revm_pinned", 5000.0, 1.0),
                ratio_row("rex5_oracle", 3500.0, 0.7),
            ],
        },
    ];
    let table = build_compare_table(
        &rows,
        &ratios,
        None,
        "rex5",
        "revm_pinned",
        &["revm_pinned".to_string()],
        |s| s.starts_with("rex5"),
    );
    std::fs::write(out.join("compare_table.json"), serde_json::to_string_pretty(&table).unwrap())
        .unwrap();

    // Built from the whole run's subject set, exactly like the pipeline does.
    let colors = SubjectColors::new(
        "revm_pinned",
        rows.iter().map(|r| r.subject.clone()).chain(["rex5_oracle".to_string()]),
        |s| s.starts_with("rex5"),
    );

    let items = vec![
        SpeedBarItem {
            item: "salt_dynamic_gas/sstore_100".into(),
            bars: vec![
                ("revm_pinned".into(), 100.0),
                ("rex5".into(), 58.0),
                ("rex5_salt".into(), 48.0),
            ],
        },
        SpeedBarItem {
            item: "oracle_real_data/oracle_sload_50".into(),
            bars: vec![("revm_pinned".into(), 100.0), ("rex5_oracle".into(), 143.0)],
        },
    ];
    render_speed_bars(
        &out.join("compare_bars.png"),
        "mega-evm relative speed (revm_pinned = 100%)",
        "relative speed, revm_pinned = 100% (lower = more overhead)",
        &items,
        &colors,
    )
    .unwrap();

    let violin_rows = [
        mk_row("salt_dynamic_gas", "revm_pinned", "sstore_100", 13970.0),
        mk_row("salt_dynamic_gas", "rex5", "sstore_100", 24040.0),
        mk_row("salt_dynamic_gas", "rex5_salt", "sstore_100", 28870.0),
    ];
    render_violin(
        &out.join("dist.png"),
        "salt_dynamic_gas/sstore_100 — per-call distribution",
        &violin_rows.iter().collect::<Vec<_>>(),
        &colors,
    )
    .unwrap();

    // Mirrors the shape of a real 5-commit digest: a dense 1.9-2.0 cluster,
    // one improving row, one alert ring, one gap.
    let commits: Vec<String> = (0..5).map(|i| format!("{:07x}", 0x1234567 + i * 0x1111)).collect();
    let mk = |label: &str, ratios: Vec<Option<f64>>, alerts: Vec<bool>| TrendSeries {
        label: label.into(),
        ratios,
        alerts,
    };
    let series = vec![
        mk(
            "salt_dynamic_gas/rex5_salt/sstore_100",
            vec![Some(2.37), Some(2.32), Some(2.34), Some(2.28), Some(2.44)],
            vec![false, false, false, false, true],
        ),
        mk(
            "empty_transaction/rex5",
            vec![Some(2.03), Some(2.08), Some(2.01), Some(2.09), Some(2.05)],
            Vec::new(),
        ),
        mk(
            "simple_ether_transfer/rex5",
            vec![Some(2.50), Some(1.98), Some(1.86), Some(1.87), Some(1.88)],
            Vec::new(),
        ),
        mk(
            "salt_dynamic_gas/rex5/sstore_100",
            vec![Some(1.99), Some(1.93), Some(1.95), Some(1.94), Some(1.95)],
            Vec::new(),
        ),
        mk(
            "sstore_heavy/rex5/sstore_100",
            vec![Some(1.99), Some(1.97), Some(1.94), Some(1.93), Some(1.94)],
            Vec::new(),
        ),
        mk(
            "subcall_1000_transfer_1wei/rex5",
            vec![Some(1.98), Some(1.96), None, Some(1.92), Some(1.93)],
            Vec::new(),
        ),
        mk(
            "subcall_1000_no_value/rex5",
            vec![Some(1.87), Some(1.76), Some(1.76), Some(1.79), Some(1.81)],
            Vec::new(),
        ),
        mk(
            "sstore_heavy/rex5/sstore_sload_100",
            vec![Some(1.79), Some(1.76), Some(1.77), Some(1.73), Some(1.73)],
            Vec::new(),
        ),
    ];
    render_trend(
        &out.join("trend.png"),
        "mega-evm headline (rex5, rex5_*) — last 5 commits",
        "time ratio vs revm_pinned — lower is better",
        &commits,
        &series,
    )
    .unwrap();
    println!("rendered to {}", out.display());
}
