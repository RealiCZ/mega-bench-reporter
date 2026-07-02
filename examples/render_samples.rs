//! Throwaway visual check: renders one of each chart type into OUT_DIR arg.
use mega_bench_reporter::charts::*;
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
        ratio_vs_revm_pinned: Some(ratio),
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
    let table = build_compare_table(&rows, &ratios, "rex5", |s| s.starts_with("rex5"));
    std::fs::write(out.join("compare_table.json"), serde_json::to_string_pretty(&table).unwrap())
        .unwrap();

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
        &items,
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
    )
    .unwrap();

    let commits: Vec<String> = (0..10).map(|i| format!("{:07x}", 0x1234567 + i * 0x1111)).collect();
    let series = vec![
        TrendSeries {
            label: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
            ratios: vec![
                Some(2.02),
                Some(2.05),
                Some(2.01),
                Some(2.04),
                Some(2.35),
                Some(2.36),
                Some(2.05),
                Some(2.03),
                Some(2.04),
                Some(2.02),
            ],
            alerts: (0..10).map(|i| i == 4).collect(),
        },
        TrendSeries {
            label: "empty_transaction/rex5".into(),
            ratios: vec![
                Some(1.18),
                Some(1.19),
                Some(1.17),
                None,
                Some(1.18),
                Some(1.20),
                Some(1.19),
                Some(1.18),
                Some(1.17),
                Some(1.18),
            ],
            alerts: Vec::new(),
        },
    ];
    render_trend(&out.join("trend.png"), "mega-evm 10-commit headline trend", &commits, &series)
        .unwrap();
    println!("rendered to {}", out.display());
}
