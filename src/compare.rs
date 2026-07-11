//! The per-commit comparison table, emitted as `compare_table.json`: one row
//! per test item, one column per implementation, last column = the headline
//! family's worst time ratio vs the baseline subject. Structured data for the
//! relaying agent to assemble into a native table — a derived record, not a
//! chart, which is why it lives apart from the plotters renderers.

use crate::criterion_results::{Row, WorkloadRatios};
use crate::instructions::InstrWorkloadRatios;
use std::collections::BTreeMap;

/// p95 of a sample set (nearest-rank).
pub fn p95(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return f64::NAN;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// The comparison table: one row per test item, one column per
/// implementation, last column = the headline family's time ratio vs the
/// baseline subject.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompareTable {
    /// Column order: configured `subject_order` first, then alphabetical.
    pub subjects: Vec<String>,
    pub rows: Vec<CompareTableRow>,
    /// Label of the ratio column, e.g. `rex5, rex5_*`.
    pub headline_label: String,
    /// The subject every ratio is against.
    pub baseline_subject: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompareTableRow {
    /// `group[/workload]`.
    pub item: String,
    /// p95 per-call µs per subject column (`None` = subject absent).
    pub p95_us: Vec<Option<f64>>,
    /// Worst (max) headline-family time ratio vs the baseline for this item.
    pub headline_ratio: Option<f64>,
    /// Instruction count per subject column, aligned with `subjects` like
    /// `p95_us`. Absent (not `null`) when the instructions lane produced
    /// nothing for this item, keeping lane-off tables byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instr: Option<Vec<Option<u64>>>,
    /// Worst (max) headline-family instruction-count ratio vs the baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instr_headline_ratio: Option<f64>,
}

fn subject_rank(subject: &str, order: &[String]) -> (usize, String) {
    match order.iter().position(|s| s == subject) {
        Some(i) => (i, String::new()),
        None => (order.len(), subject.to_string()),
    }
}

/// The ratio column's aggregation, shared by both lanes: the worst (max) of
/// an item's headline-family ratios, `None` when no headline row has one.
fn worst_ratio(ratios: impl Iterator<Item = f64>) -> Option<f64> {
    ratios.reduce(f64::max)
}

/// Assembles the comparison table from parsed rows + ratio tables.
/// `is_headline` decides which subjects feed the ratio column;
/// `subject_order` pins the leading columns (unlisted subjects follow
/// alphabetically) and is display-only — `baseline_subject` is the
/// configured ratio baseline regardless of column order. `instr_ratios`
/// (the instructions lane, `None` when off) adds per-subject counts and a
/// headline count ratio to matching items; the column set stays driven by
/// the walltime rows.
pub fn build_compare_table(
    rows: &[Row],
    ratios: &[WorkloadRatios],
    instr_ratios: Option<&[InstrWorkloadRatios]>,
    headline_label: &str,
    baseline_subject: &str,
    subject_order: &[String],
    is_headline: impl Fn(&str) -> bool,
) -> CompareTable {
    let mut subjects: Vec<String> = rows
        .iter()
        .map(|r| r.subject.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    subjects.sort_by_key(|s| subject_rank(s, subject_order));

    // (group, workload) -> subject -> p95 µs.
    let mut p95_by_item: BTreeMap<(String, String), BTreeMap<String, f64>> = BTreeMap::new();
    for row in rows {
        p95_by_item
            .entry((row.group.clone(), row.workload.clone()))
            .or_default()
            .insert(row.subject.clone(), p95(&row.samples_ns) / 1000.0);
    }

    // (group, workload) -> the instructions lane's ratio table for that item.
    let instr_by_item: BTreeMap<(String, String), &InstrWorkloadRatios> = instr_ratios
        .unwrap_or_default()
        .iter()
        .map(|wl| ((wl.group.clone(), wl.workload.clone()), wl))
        .collect();

    let table_rows = ratios
        .iter()
        .map(|wl| {
            let item = if wl.workload.is_empty() {
                wl.group.clone()
            } else {
                format!("{}/{}", wl.group, wl.workload)
            };
            let p95_map = p95_by_item.get(&(wl.group.clone(), wl.workload.clone()));
            let p95_us = subjects.iter().map(|s| p95_map.and_then(|m| m.get(s)).copied()).collect();
            let headline_ratio = worst_ratio(
                wl.rows
                    .iter()
                    .filter(|r| is_headline(&r.subject))
                    .filter_map(|r| r.ratio_vs_baseline),
            );
            let instr_wl = instr_by_item.get(&(wl.group.clone(), wl.workload.clone()));
            let instr = instr_wl.map(|iwl| {
                subjects
                    .iter()
                    .map(|s| iwl.rows.iter().find(|r| &r.subject == s).map(|r| r.count))
                    .collect()
            });
            let instr_headline_ratio = instr_wl.and_then(|iwl| {
                worst_ratio(
                    iwl.rows
                        .iter()
                        .filter(|r| is_headline(&r.subject))
                        .filter_map(|r| r.ratio_vs_baseline),
                )
            });
            CompareTableRow { item, p95_us, headline_ratio, instr, instr_headline_ratio }
        })
        .collect();

    CompareTable {
        subjects,
        rows: table_rows,
        headline_label: headline_label.to_string(),
        baseline_subject: baseline_subject.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criterion_results::RatioRow;

    fn fake_samples(center_ns: f64, n: usize) -> Vec<f64> {
        // Deterministic pseudo-noise around the center; no rand dependency.
        (0..n).map(|i| center_ns * (1.0 + 0.02 * ((i as f64 * 2.399).sin()))).collect()
    }

    fn sample_ratio_rows() -> (Vec<Row>, Vec<WorkloadRatios>) {
        let mk = |subject: &str, mean: f64| Row {
            group: "salt_dynamic_gas".into(),
            subject: subject.into(),
            workload: "sstore_100".into(),
            mean_ns: mean,
            std_dev_ns: mean * 0.01,
            samples_ns: fake_samples(mean, 50),
        };
        let rows = vec![mk("revm_pinned", 14000.0), mk("rex4", 20000.0), mk("rex5_salt", 28000.0)];
        let ratios = vec![WorkloadRatios {
            group: "salt_dynamic_gas".into(),
            workload: "sstore_100".into(),
            rows: vec![
                RatioRow {
                    subject: "revm_pinned".into(),
                    mean_ns: 14000.0,
                    ratio_vs_baseline: Some(1.0),
                },
                RatioRow {
                    subject: "rex4".into(),
                    mean_ns: 20000.0,
                    ratio_vs_baseline: Some(1.43),
                },
                RatioRow {
                    subject: "rex5_salt".into(),
                    mean_ns: 28000.0,
                    ratio_vs_baseline: Some(2.0),
                },
            ],
        }];
        (rows, ratios)
    }

    #[test]
    fn test_p95_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(p95(&samples), 95.0);
        assert_eq!(p95(&[7.0]), 7.0);
        assert!(p95(&[]).is_nan());
    }

    #[test]
    fn test_build_compare_table_orders_subjects_and_picks_worst_headline_ratio() {
        let (rows, ratios) = sample_ratio_rows();
        let table = build_compare_table(
            &rows,
            &ratios,
            None,
            "rex5",
            "revm_pinned",
            &["revm_pinned".to_string()],
            |s| s.starts_with("rex5"),
        );
        assert_eq!(table.subjects, vec!["revm_pinned", "rex4", "rex5_salt"]);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0].item, "salt_dynamic_gas/sstore_100");
        // Worst headline ratio (only rex5_salt matches the filter).
        assert_eq!(table.rows[0].headline_ratio, Some(2.0));
        assert!(table.rows[0].p95_us.iter().all(|v| v.is_some()));
        // Lane off: the instr fields stay absent from the serialized table.
        assert_eq!(table.rows[0].instr, None);
        assert_eq!(table.rows[0].instr_headline_ratio, None);
        let json = serde_json::to_string(&table).unwrap();
        assert!(!json.contains("instr"), "lane-off table must not mention instr: {json}");
    }

    #[test]
    fn test_build_compare_table_instr_columns_align_with_subjects() {
        let (rows, ratios) = sample_ratio_rows();
        let instr = vec![crate::instructions::InstrWorkloadRatios {
            group: "salt_dynamic_gas".into(),
            workload: "sstore_100".into(),
            rows: vec![
                crate::instructions::InstrRatioRow {
                    subject: "revm_pinned".into(),
                    count: 10_000,
                    ratio_vs_baseline: Some(1.0),
                },
                // rex4 has no instr row (absent column); rex5_salt does.
                crate::instructions::InstrRatioRow {
                    subject: "rex5_salt".into(),
                    count: 25_000,
                    ratio_vs_baseline: Some(2.5),
                },
            ],
        }];
        let table = build_compare_table(
            &rows,
            &ratios,
            Some(&instr),
            "rex5",
            "revm_pinned",
            &["revm_pinned".to_string()],
            |s| s.starts_with("rex5"),
        );
        assert_eq!(table.subjects, vec!["revm_pinned", "rex4", "rex5_salt"]);
        assert_eq!(table.rows[0].instr, Some(vec![Some(10_000), None, Some(25_000)]));
        assert_eq!(table.rows[0].instr_headline_ratio, Some(2.5));
        // The walltime columns are untouched by the extra lane.
        assert_eq!(table.rows[0].headline_ratio, Some(2.0));
    }

    #[test]
    fn test_build_compare_table_baseline_label_independent_of_display_order() {
        // subject_order is display-only: a configured order that doesn't lead
        // with the baseline must not relabel the table's baseline_subject.
        let (rows, ratios) = sample_ratio_rows();
        let table = build_compare_table(
            &rows,
            &ratios,
            None,
            "rex5",
            "revm_pinned",
            &["rex5_salt".to_string(), "rex4".to_string()],
            |s| s.starts_with("rex5"),
        );
        assert_eq!(table.baseline_subject, "revm_pinned");
        assert_eq!(table.subjects, vec!["rex5_salt", "rex4", "revm_pinned"]);
    }
}
