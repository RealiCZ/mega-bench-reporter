//! Throwaway visual check: renders one of each chart into OUT_DIR arg.
use mega_bench_reporter::charts::*;
use mega_bench_reporter::criterion_results::Row;
use std::path::Path;

fn main() {
    let out = std::env::args().nth(1).expect("usage: render_samples <out-dir>");
    let out = Path::new(&out);
    std::fs::create_dir_all(out).unwrap();

    let bars = vec![
        CompareBar { label: "salt_dynamic_gas/sstore_100 · rex5_salt".into(), ratio: 2.07 },
        CompareBar { label: "salt_dynamic_gas/sstore_100 · rex5".into(), ratio: 1.72 },
        CompareBar { label: "oracle_real_data/oracle_sload_50 · rex5".into(), ratio: 0.97 },
        CompareBar { label: "empty_transaction · rex5".into(), ratio: 1.18 },
        CompareBar { label: "comp_cost/modexp · rex5".into(), ratio: 1.04 },
    ];
    render_compare_bar(&out.join("compare.png"), "mega-evm vs revm @ abc1234", &bars).unwrap();

    let mk = |subject: &str, center: f64| Row {
        group: "salt_dynamic_gas".into(),
        subject: subject.into(),
        workload: "sstore_100".into(),
        mean_ns: center,
        std_dev_ns: center * 0.015,
        samples_ns: (0..100)
            .map(|i| {
                center
                    * (1.0 + 0.015 * ((i as f64 * 2.399).sin()) + 0.01 * ((i as f64 * 0.71).cos()))
            })
            .collect(),
    };
    let rows = [mk("revm_pinned", 13970.0), mk("rex5", 24040.0), mk("rex5_salt", 28870.0)];
    render_violin(
        &out.join("dist.png"),
        "salt_dynamic_gas/sstore_100 — per-call distribution",
        &rows.iter().collect::<Vec<_>>(),
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
        },
    ];
    render_trend(&out.join("trend.png"), "mega-evm 10-commit headline trend", &commits, &series)
        .unwrap();
    println!("rendered to {}", out.display());
}
