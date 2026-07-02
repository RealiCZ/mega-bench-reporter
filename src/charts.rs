//! Chart rendering with `plotters` (D5): comparison bar chart, violin
//! (distribution) plot, and the digest trend chart — all PNG, all using an
//! embedded font so the binary has no system font/fontconfig dependency.
//!
//! The violin shape/interpretation was validated against real criterion
//! `sample.json` data with a Python prototype; this is the `plotters`
//! re-implementation of the same approach.

use crate::criterion_results::{Row, WorkloadRatios};
use plotters::prelude::*;
use plotters::style::register_font;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Once;

/// Embedded DejaVu Sans (see `assets/fonts/DEJAVU-LICENSE`) — registered as
/// `sans-serif` so chart text renders identically on any machine, with no
/// fontconfig/system-font lookup.
const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

fn ensure_font() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_font("sans-serif", FontStyle::Normal, FONT_BYTES)
            .unwrap_or_else(|_| panic!("embedded DejaVuSans.ttf is not a valid font"));
    });
}

/// matplotlib tab10-ish palette, one color per series.
const PALETTE: &[RGBColor] = &[
    RGBColor(31, 119, 180),
    RGBColor(255, 127, 14),
    RGBColor(44, 160, 44),
    RGBColor(214, 39, 40),
    RGBColor(148, 103, 189),
    RGBColor(140, 86, 75),
    RGBColor(227, 119, 194),
    RGBColor(127, 127, 127),
    RGBColor(188, 189, 34),
    RGBColor(23, 190, 207),
];

fn series_color(i: usize) -> RGBColor {
    PALETTE[i % PALETTE.len()]
}

fn map_err<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("chart rendering: {e}")
}

/// Maps near-integer tick positions to `labels[n - 1 - i]` (top-down order),
/// everything else to an empty label.
fn integer_tick_label(v: f64, labels: &[&str]) -> String {
    let i = v.round();
    if (v - i).abs() > 0.01 || i < 0.0 {
        return String::new();
    }
    let i = i as usize;
    if i < labels.len() {
        labels[labels.len() - 1 - i].to_string()
    } else {
        String::new()
    }
}

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

/// The comparison table (the design doc's table view): one row per test item,
/// one column per implementation, last column = the headline family's time
/// ratio vs `revm_pinned`. Emitted as `compare_table.json` for the relaying
/// agent to assemble into a native Lark table.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompareTable {
    /// Column order: baselines first, then specs, then variants.
    pub subjects: Vec<String>,
    pub rows: Vec<CompareTableRow>,
    /// Label of the ratio column, e.g. `rex5`.
    pub headline_label: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompareTableRow {
    /// `group[/workload]`.
    pub item: String,
    /// p95 per-call µs per subject column (`None` = subject absent).
    pub p95_us: Vec<Option<f64>>,
    /// Worst (max) headline-family time ratio vs revm_pinned for this item.
    pub headline_ratio: Option<f64>,
}

/// Fixed display order for well-known subjects; everything else goes after,
/// alphabetically.
const SUBJECT_ORDER: &[&str] = &[
    "revm_pinned",
    "revm_latest",
    "op_revm_pinned",
    "op_revm_latest",
    "equivalence",
    "mini_rex",
    "rex",
    "rex1",
    "rex2",
    "rex3",
    "rex4",
    "rex5",
];

fn subject_rank(subject: &str) -> (usize, String) {
    match SUBJECT_ORDER.iter().position(|s| *s == subject) {
        Some(i) => (i, String::new()),
        None => (SUBJECT_ORDER.len(), subject.to_string()),
    }
}

/// Assembles the comparison table from parsed rows + ratio tables.
/// `is_headline` decides which subjects feed the ratio column.
pub fn build_compare_table(
    rows: &[Row],
    ratios: &[WorkloadRatios],
    headline_label: &str,
    is_headline: impl Fn(&str) -> bool,
) -> CompareTable {
    let mut subjects: Vec<String> = rows
        .iter()
        .map(|r| r.subject.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    subjects.sort_by_key(|s| subject_rank(s));

    // (group, workload) -> subject -> p95 µs.
    let mut p95_by_item: BTreeMap<(String, String), BTreeMap<String, f64>> = BTreeMap::new();
    for row in rows {
        p95_by_item
            .entry((row.group.clone(), row.workload.clone()))
            .or_default()
            .insert(row.subject.clone(), p95(&row.samples_ns) / 1000.0);
    }

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
            let headline_ratio = wl
                .rows
                .iter()
                .filter(|r| is_headline(&r.subject))
                .filter_map(|r| r.ratio_vs_revm_pinned)
                .fold(None, |acc: Option<f64>, r| Some(acc.map_or(r, |a| a.max(r))));
            CompareTableRow { item, p95_us, headline_ratio }
        })
        .collect();

    CompareTable { subjects, rows: table_rows, headline_label: headline_label.to_string() }
}

/// One item of the speed bar chart: baseline plus each headline subject's
/// relative speed (`100 × baseline_time / subject_time`; revm_pinned = 100%,
/// lower = more overhead — the design mock's revm=100% bar view).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedBarItem {
    pub item: String,
    /// `(subject, percent)`, baseline first.
    pub bars: Vec<(String, f64)>,
}

/// Grouped horizontal bar chart, one group per test item (horizontal because
/// real item names like `salt_dynamic_gas/sstore_100` are far too long for
/// the mock's vertical layout).
pub fn render_speed_bars(path: &Path, title: &str, items: &[SpeedBarItem]) -> anyhow::Result<()> {
    ensure_font();
    if items.is_empty() {
        anyhow::bail!("no items to render speed bars for");
    }
    // Legend order = subject order of first appearance.
    let mut legend_subjects: Vec<String> = Vec::new();
    for item in items {
        for (subject, _) in &item.bars {
            if !legend_subjects.contains(subject) {
                legend_subjects.push(subject.clone());
            }
        }
    }
    let subject_color = |subject: &str| -> RGBColor {
        if subject == "revm_pinned" {
            RGBColor(144, 153, 176) // neutral gray-blue baseline
        } else {
            let i = legend_subjects.iter().position(|s| s == subject).unwrap_or(0);
            series_color(i)
        }
    };

    let band = legend_subjects.len().max(1) as f64 + 1.0; // bars + gap per item
    let total = items.len() as f64 * band - 1.0;
    let max_percent =
        items.iter().flat_map(|i| i.bars.iter().map(|(_, p)| *p)).fold(100.0_f64, f64::max);
    let x_max = (max_percent * 1.15).max(118.0);
    let height =
        (170 + (items.len() + 1) * (legend_subjects.len() * 22 + 14)).clamp(300, 2200) as u32;
    // Reserve room below the last item for the legend box, in *data units*
    // derived from its pixel size — stays correct when the height clamp above
    // shrinks the pixels-per-band.
    let legend_px = 22.0 * legend_subjects.len() as f64 + 18.0;
    let plot_px = (height as f64 - 130.0).max(100.0);
    let legend_space = (legend_px * (total + 1.0) / (plot_px - legend_px).max(100.0)).max(1.2);
    let root = BitMapBackend::new(path, (1100, height)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;

    let labels: Vec<&str> = items.iter().map(|i| i.item.as_str()).collect();
    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("sans-serif", 22))
        .margin(15)
        .x_label_area_size(45)
        .y_label_area_size(340)
        .build_cartesian_2d(0.0..x_max, (-0.6 - legend_space)..(total + 0.6))
        .map_err(map_err)?;

    let band_center = |i: usize| -> f64 {
        // Item 0 at the top.
        let base = (items.len() - 1 - i) as f64 * band;
        base + (legend_subjects.len() as f64 - 1.0) / 2.0
    };
    chart
        .configure_mesh()
        .disable_y_mesh()
        .light_line_style(TRANSPARENT)
        .bold_line_style(BLACK.mix(0.12))
        .y_labels(((total + legend_space) as usize) * 4 + 5)
        .y_label_formatter(&|v: &f64| {
            for (i, label) in labels.iter().enumerate() {
                if (v - band_center(i)).abs() <= 0.26 {
                    return label.to_string();
                }
            }
            String::new()
        })
        .x_desc("relative speed, revm_pinned = 100% (lower = more overhead)")
        .label_style(("sans-serif", 14))
        .axis_desc_style(("sans-serif", 16))
        .draw()
        .map_err(map_err)?;

    for (i, item) in items.iter().enumerate() {
        let base = (items.len() - 1 - i) as f64 * band;
        for (b, (subject, percent)) in item.bars.iter().enumerate() {
            // Bar 0 (baseline) at the top of the band.
            let y = base + (legend_subjects.len() - 1 - b.min(legend_subjects.len() - 1)) as f64;
            let color = subject_color(subject);
            chart
                .draw_series(std::iter::once(Rectangle::new(
                    [(0.0, y - 0.42), (*percent, y + 0.42)],
                    color.mix(0.9).filled(),
                )))
                .map_err(map_err)?;
            chart
                .draw_series(std::iter::once(Text::new(
                    format!("{percent:.0}%"),
                    (percent + 1.2, y - 0.18),
                    ("sans-serif", 13).into_font().color(&BLACK),
                )))
                .map_err(map_err)?;
        }
    }

    // 100% reference line.
    chart
        .draw_series(LineSeries::new(
            [(100.0, -0.6 - legend_space), (100.0, total + 0.6)],
            BLACK.mix(0.45).stroke_width(1),
        ))
        .map_err(map_err)?;

    // Legend entries (empty series, label only).
    for subject in &legend_subjects {
        let color = subject_color(subject);
        chart
            .draw_series(LineSeries::new(std::iter::empty::<(f64, f64)>(), color.filled()))
            .map_err(map_err)?
            .label(subject.clone())
            .legend(move |(x, y)| Rectangle::new([(x, y - 5), (x + 10, y + 5)], color.filled()));
    }
    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::LowerLeft)
        .background_style(WHITE.mix(0.85))
        .border_style(BLACK.mix(0.4))
        .label_font(("sans-serif", 14))
        .draw()
        .map_err(map_err)?;

    root.present().map_err(map_err)?;
    Ok(())
}

/// Gaussian KDE over `grid` with Silverman's rule-of-thumb bandwidth — the
/// same estimator the validated Python (scipy) prototype used.
fn gaussian_kde(samples: &[f64], grid: &[f64]) -> Vec<f64> {
    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let var = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
    let sd = var.sqrt();
    // Degenerate spread (all-equal samples) still needs a nonzero bandwidth.
    let bw = (1.06 * sd * n.powf(-0.2)).max(mean.abs() * 1e-4).max(1e-12);
    grid.iter()
        .map(|x| {
            samples
                .iter()
                .map(|s| {
                    let u = (x - s) / bw;
                    (-0.5 * u * u).exp()
                })
                .sum::<f64>()
                / (n * bw * (2.0 * std::f64::consts::PI).sqrt())
        })
        .collect()
}

/// Horizontal violin plot for one `(group, workload)`: one violin per subject,
/// x = per-call time in µs, with min–max whiskers and a mean tick — the same
/// layout as the validated `fig-dist-real.png` prototype.
pub fn render_violin(path: &Path, title: &str, rows: &[&Row]) -> anyhow::Result<()> {
    ensure_font();
    if rows.is_empty() {
        anyhow::bail!("no rows to render a violin for");
    }
    if rows.iter().any(|r| r.samples_ns.is_empty()) {
        anyhow::bail!("a row has no samples");
    }
    let n = rows.len();

    // Per-call µs samples per row.
    let samples_us: Vec<Vec<f64>> =
        rows.iter().map(|r| r.samples_ns.iter().map(|ns| ns / 1000.0).collect()).collect();
    let x_min = samples_us.iter().flatten().copied().fold(f64::INFINITY, f64::min);
    let x_max = samples_us.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max);
    let pad = ((x_max - x_min) * 0.06).max(x_max.abs() * 1e-3).max(1e-9);
    let (x_lo, x_hi) = (x_min - pad, x_max + pad);

    let labels: Vec<String> = rows
        .iter()
        .zip(&samples_us)
        .map(|(r, s)| {
            let mean = s.iter().sum::<f64>() / s.len() as f64;
            let cv = if mean != 0.0 { (r.std_dev_ns / 1000.0) / mean * 100.0 } else { 0.0 };
            format!("{} ({mean:.2} µs, CV {cv:.1}%)", r.subject)
        })
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

    let height = (120 + n * 110).clamp(260, 1200) as u32;
    let root = BitMapBackend::new(path, (1100, height)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;

    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("sans-serif", 22))
        .margin(15)
        .x_label_area_size(45)
        .y_label_area_size(330)
        .build_cartesian_2d(x_lo..x_hi, -0.6..(n as f64 - 0.4))
        .map_err(map_err)?;

    chart
        .configure_mesh()
        .disable_y_mesh()
        .light_line_style(TRANSPARENT)
        .bold_line_style(BLACK.mix(0.12))
        .y_labels(n * 2 + 1)
        .y_label_formatter(&|v: &f64| integer_tick_label(*v, &label_refs))
        .x_desc("per-call time (µs)")
        .label_style(("sans-serif", 15))
        .axis_desc_style(("sans-serif", 16))
        .draw()
        .map_err(map_err)?;

    let y_of = |i: usize| (n - 1 - i) as f64;
    for (i, samples) in samples_us.iter().enumerate() {
        let color = series_color(i);
        let y = y_of(i);

        let lo = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let grid: Vec<f64> = (0..=200).map(|k| lo + (hi - lo) * k as f64 / 200.0).collect();
        let density = gaussian_kde(samples, &grid);
        let d_max = density.iter().copied().fold(f64::MIN, f64::max).max(1e-300);
        let half = 0.38;

        // Violin body: upper contour then mirrored lower contour.
        let mut polygon: Vec<(f64, f64)> =
            grid.iter().zip(&density).map(|(x, d)| (*x, y + d / d_max * half)).collect();
        polygon.extend(grid.iter().zip(&density).rev().map(|(x, d)| (*x, y - d / d_max * half)));
        chart
            .draw_series(std::iter::once(Polygon::new(polygon, color.mix(0.45).filled())))
            .map_err(map_err)?;

        // min–max whisker and a mean tick.
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        chart
            .draw_series(LineSeries::new([(lo, y), (hi, y)], color.stroke_width(1)))
            .map_err(map_err)?;
        chart
            .draw_series(LineSeries::new(
                [(mean, y - half * 0.7), (mean, y + half * 0.7)],
                color.stroke_width(2),
            ))
            .map_err(map_err)?;
    }

    root.present().map_err(map_err)?;
    Ok(())
}

/// One trend series: a row's ratio across the digest's commits (a `None` skips
/// that commit's point, e.g. the row was missing that run).
#[derive(Debug, Clone, PartialEq)]
pub struct TrendSeries {
    pub label: String,
    pub ratios: Vec<Option<f64>>,
    /// Same length as `ratios` (or empty = no markers): `true` marks a point
    /// that tripped the regression threshold vs the window's prior median —
    /// drawn as a red ring on the trend chart.
    pub alerts: Vec<bool>,
}

/// Digest trend chart: headline-row ratios over the last N commits,
/// x = commit (short sha), y = `× vs revm_pinned`.
pub fn render_trend(
    path: &Path,
    title: &str,
    commit_labels: &[String],
    series: &[TrendSeries],
) -> anyhow::Result<()> {
    ensure_font();
    if series.is_empty() || commit_labels.is_empty() {
        anyhow::bail!("no trend data to render");
    }
    let all: Vec<f64> = series.iter().flat_map(|s| s.ratios.iter().flatten().copied()).collect();
    if all.is_empty() {
        anyhow::bail!("trend series contain no ratio points");
    }
    let y_min = all.iter().copied().fold(f64::INFINITY, f64::min);
    let y_max = all.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let pad = ((y_max - y_min) * 0.15).max(y_max.abs() * 0.02).max(1e-9);
    let (y_lo, y_hi) = ((y_min - pad).min(1.0 - pad), y_max + pad);

    let n = commit_labels.len();
    let label_refs: Vec<&str> = commit_labels.iter().map(|s| s.as_str()).collect();
    let root = BitMapBackend::new(path, (1100, 480)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;

    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("sans-serif", 22))
        .margin(15)
        .x_label_area_size(50)
        .y_label_area_size(70)
        .build_cartesian_2d(-0.5..(n as f64 - 0.5), y_lo..y_hi)
        .map_err(map_err)?;

    chart
        .configure_mesh()
        .light_line_style(TRANSPARENT)
        .bold_line_style(BLACK.mix(0.12))
        .x_labels(n * 2 + 1)
        .x_label_formatter(&|v: &f64| {
            let i = v.round();
            if (v - i).abs() > 0.01 || i < 0.0 {
                return String::new();
            }
            label_refs.get(i as usize).map(|s| s.to_string()).unwrap_or_default()
        })
        .y_label_formatter(&|v: &f64| format!("{v:.2}×"))
        .x_desc("commit")
        .y_desc("× vs revm_pinned")
        .label_style(("sans-serif", 13))
        .axis_desc_style(("sans-serif", 16))
        .draw()
        .map_err(map_err)?;

    // 1.0× parity reference.
    chart
        .draw_series(LineSeries::new(
            [(-0.5, 1.0), (n as f64 - 0.5, 1.0)],
            BLACK.mix(0.4).stroke_width(1),
        ))
        .map_err(map_err)?;

    for (i, s) in series.iter().enumerate() {
        let color = series_color(i);
        let points: Vec<(f64, f64)> =
            s.ratios.iter().enumerate().filter_map(|(x, r)| r.map(|r| (x as f64, r))).collect();
        chart
            .draw_series(LineSeries::new(points.clone(), color.stroke_width(2)))
            .map_err(map_err)?
            .label(s.label.clone())
            .legend(move |(x, y)| Rectangle::new([(x, y - 5), (x + 10, y + 5)], color.filled()));
        chart
            .draw_series(points.iter().map(|(x, y)| Circle::new((*x, *y), 3, color.filled())))
            .map_err(map_err)?;
        // Red ring on the points that tripped the regression threshold.
        let alert_points: Vec<(f64, f64)> = s
            .ratios
            .iter()
            .enumerate()
            .filter(|(x, _)| s.alerts.get(*x).copied().unwrap_or(false))
            .filter_map(|(x, r)| r.map(|r| (x as f64, r)))
            .collect();
        chart
            .draw_series(
                alert_points
                    .iter()
                    .map(|(x, y)| Circle::new((*x, *y), 7, RGBColor(214, 39, 40).stroke_width(2))),
            )
            .map_err(map_err)?;
    }

    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperRight)
        .background_style(WHITE.mix(0.85))
        .border_style(BLACK.mix(0.4))
        .label_font(("sans-serif", 14))
        .draw()
        .map_err(map_err)?;

    root.present().map_err(map_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criterion_results::RatioRow;

    fn assert_png(path: &Path) {
        let bytes = std::fs::read(path).expect("chart file written");
        assert!(bytes.len() > 1000, "suspiciously small png ({} bytes)", bytes.len());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a png");
    }

    fn fake_samples(center_ns: f64, n: usize) -> Vec<f64> {
        // Deterministic pseudo-noise around the center; no rand dependency.
        (0..n).map(|i| center_ns * (1.0 + 0.02 * ((i as f64 * 2.399).sin()))).collect()
    }

    #[test]
    fn test_p95_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(p95(&samples), 95.0);
        assert_eq!(p95(&[7.0]), 7.0);
        assert!(p95(&[]).is_nan());
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
                    ratio_vs_revm_pinned: Some(1.0),
                },
                RatioRow {
                    subject: "rex4".into(),
                    mean_ns: 20000.0,
                    ratio_vs_revm_pinned: Some(1.43),
                },
                RatioRow {
                    subject: "rex5_salt".into(),
                    mean_ns: 28000.0,
                    ratio_vs_revm_pinned: Some(2.0),
                },
            ],
        }];
        (rows, ratios)
    }

    #[test]
    fn test_build_compare_table_orders_subjects_and_picks_worst_headline_ratio() {
        let (rows, ratios) = sample_ratio_rows();
        let table = build_compare_table(&rows, &ratios, "rex5", |s| s.starts_with("rex5"));
        assert_eq!(table.subjects, vec!["revm_pinned", "rex4", "rex5_salt"]);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0].item, "salt_dynamic_gas/sstore_100");
        // Worst headline ratio (only rex5_salt matches the filter).
        assert_eq!(table.rows[0].headline_ratio, Some(2.0));
        assert!(table.rows[0].p95_us.iter().all(|v| v.is_some()));
    }

    #[test]
    fn test_render_speed_bars_writes_png() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("compare_bars.png");
        let items = vec![
            SpeedBarItem {
                item: "salt_dynamic_gas/sstore_100".into(),
                bars: vec![
                    ("revm_pinned".into(), 100.0),
                    ("rex5".into(), 57.0),
                    ("rex5_salt".into(), 48.0),
                ],
            },
            SpeedBarItem {
                item: "oracle_real_data/oracle_sload_50".into(),
                bars: vec![("revm_pinned".into(), 100.0), ("rex5_oracle".into(), 143.0)],
            },
        ];
        render_speed_bars(&path, "relative speed (revm_pinned = 100%)", &items).unwrap();
        assert_png(&path);
    }

    #[test]
    fn test_render_violin_writes_png() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dist.png");
        let baseline = Row {
            group: "salt_dynamic_gas".into(),
            subject: "revm_pinned".into(),
            workload: "sstore_100".into(),
            mean_ns: 14000.0,
            std_dev_ns: 250.0,
            samples_ns: fake_samples(14000.0, 100),
        };
        let feature = Row {
            group: "salt_dynamic_gas".into(),
            subject: "rex5_salt".into(),
            workload: "sstore_100".into(),
            mean_ns: 28000.0,
            std_dev_ns: 240.0,
            samples_ns: fake_samples(28000.0, 100),
        };
        render_violin(&path, "salt_dynamic_gas/sstore_100", &[&baseline, &feature]).unwrap();
        assert_png(&path);
    }

    #[test]
    fn test_render_violin_handles_degenerate_all_equal_samples() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dist.png");
        let row = Row {
            group: "g".into(),
            subject: "s".into(),
            workload: "w".into(),
            mean_ns: 1000.0,
            std_dev_ns: 0.0,
            samples_ns: vec![1000.0; 50],
        };
        render_violin(&path, "degenerate", &[&row]).unwrap();
        assert_png(&path);
    }

    #[test]
    fn test_gaussian_kde_integrates_to_one() {
        let samples = fake_samples(100.0, 200);
        let lo = 90.0;
        let hi = 110.0;
        let grid: Vec<f64> = (0..=1000).map(|k| lo + (hi - lo) * k as f64 / 1000.0).collect();
        let density = gaussian_kde(&samples, &grid);
        let dx = (hi - lo) / 1000.0;
        let integral: f64 = density.iter().sum::<f64>() * dx;
        assert!((integral - 1.0).abs() < 0.05, "kde integral was {integral}");
    }

    #[test]
    fn test_render_trend_writes_png() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trend.png");
        let commits: Vec<String> =
            (0..10).map(|i| format!("{:07x}", 0x1234567 + i * 0x111)).collect();
        let series = vec![
            TrendSeries {
                label: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
                ratios: (0..10).map(|i| Some(2.0 + 0.02 * i as f64)).collect(),
                alerts: (0..10).map(|i| i == 6).collect(),
            },
            TrendSeries {
                label: "empty_transaction/rex5".into(),
                // One missing commit (row absent that run) must not break the line.
                ratios: (0..10).map(|i| if i == 4 { None } else { Some(1.2) }).collect(),
                alerts: Vec::new(),
            },
        ];
        render_trend(&path, "mega-evm 10-commit trend", &commits, &series).unwrap();
        assert_png(&path);
    }
}
