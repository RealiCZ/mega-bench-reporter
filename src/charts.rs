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

/// One bar of the comparison chart: a workload's ratio for one subject.
#[derive(Debug, Clone, PartialEq)]
pub struct CompareBar {
    /// Bar label, e.g. `salt_dynamic_gas/sstore_100 · rex5_salt`.
    pub label: String,
    /// `× vs revm_pinned` (time ratio — higher is slower).
    pub ratio: f64,
}

/// Flattens ratio tables into bars for `subjects` (typically the headline-spec
/// family, e.g. `rex5`, `rex5_salt`, …). Baseline rows (ratio 1.0) and rows
/// without a baseline are omitted.
pub fn compare_bars(ratios: &[WorkloadRatios], subjects: &[String]) -> Vec<CompareBar> {
    let mut bars = Vec::new();
    for wl in ratios {
        for row in &wl.rows {
            if !subjects.contains(&row.subject) {
                continue;
            }
            let Some(ratio) = row.ratio_vs_revm_pinned else { continue };
            let workload = if wl.workload.is_empty() {
                wl.group.clone()
            } else {
                format!("{}/{}", wl.group, wl.workload)
            };
            bars.push(CompareBar { label: format!("{workload} · {}", row.subject), ratio });
        }
    }
    bars
}

/// Horizontal bar chart: one bar per `(workload, subject)`, x = `× vs
/// revm_pinned`, with a reference line at 1.0×.
pub fn render_compare_bar(path: &Path, title: &str, bars: &[CompareBar]) -> anyhow::Result<()> {
    ensure_font();
    if bars.is_empty() {
        anyhow::bail!("no bars to render (no rows with a revm_pinned baseline?)");
    }
    let n = bars.len();
    let height = (80 + n * 30).clamp(240, 1600) as u32;
    let root = BitMapBackend::new(path, (1100, height)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;

    let max_ratio = bars.iter().map(|b| b.ratio).fold(1.0_f64, f64::max);
    let x_max = max_ratio * 1.18;
    let labels: Vec<&str> = bars.iter().map(|b| b.label.as_str()).collect();

    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("sans-serif", 22))
        .margin(15)
        .x_label_area_size(45)
        .y_label_area_size(360)
        .build_cartesian_2d(0.0..x_max, -0.6..(n as f64 - 0.4))
        .map_err(map_err)?;

    chart
        .configure_mesh()
        .disable_y_mesh()
        .light_line_style(TRANSPARENT)
        .bold_line_style(BLACK.mix(0.12))
        .y_labels(n * 2 + 1)
        .y_label_formatter(&|v: &f64| integer_tick_label(*v, &labels))
        .x_desc("× vs revm_pinned (time — lower is better)")
        .label_style(("sans-serif", 15))
        .axis_desc_style(("sans-serif", 16))
        .draw()
        .map_err(map_err)?;

    // Bars are drawn top-down: bar 0 at the top row.
    let y_of = |i: usize| (n - 1 - i) as f64;
    chart
        .draw_series(bars.iter().enumerate().map(|(i, bar)| {
            let color = if bar.ratio <= 1.05 {
                RGBColor(44, 160, 44) // at or under baseline: green
            } else if bar.ratio <= 2.0 {
                RGBColor(255, 165, 0) // moderate overhead: amber
            } else {
                RGBColor(214, 39, 40) // heavy overhead: red
            };
            Rectangle::new(
                [(0.0, y_of(i) - 0.35), (bar.ratio, y_of(i) + 0.35)],
                color.mix(0.85).filled(),
            )
        }))
        .map_err(map_err)?;
    chart
        .draw_series(bars.iter().enumerate().map(|(i, bar)| {
            Text::new(
                format!("{:.2}×", bar.ratio),
                (bar.ratio + x_max * 0.01, y_of(i) - 0.12),
                ("sans-serif", 14).into_font().color(&BLACK),
            )
        }))
        .map_err(map_err)?;
    // 1.0× (parity with revm_pinned) reference line.
    chart
        .draw_series(LineSeries::new(
            [(1.0, -0.6), (1.0, n as f64 - 0.4)],
            BLACK.mix(0.45).stroke_width(1),
        ))
        .map_err(map_err)?;

    root.present().map_err(map_err)?;
    Ok(())
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
    fn test_compare_bars_selects_headline_subjects() {
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
        let bars = compare_bars(&ratios, &["rex5".to_string(), "rex5_salt".to_string()]);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].label, "salt_dynamic_gas/sstore_100 · rex5_salt");
        assert!((bars[0].ratio - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_render_compare_bar_writes_png() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("compare.png");
        let bars = vec![
            CompareBar { label: "salt_dynamic_gas/sstore_100 · rex5_salt".into(), ratio: 2.07 },
            CompareBar { label: "empty_transaction · rex5".into(), ratio: 1.18 },
            CompareBar { label: "oracle_real_data/oracle_sload_50 · rex5".into(), ratio: 0.97 },
        ];
        render_compare_bar(&path, "mega-evm vs revm @ abc1234", &bars).unwrap();
        assert_png(&path);
    }

    #[test]
    fn test_render_compare_bar_empty_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(render_compare_bar(&tmp.path().join("x.png"), "t", &[]).is_err());
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
            },
            TrendSeries {
                label: "empty_transaction/rex5".into(),
                // One missing commit (row absent that run) must not break the line.
                ratios: (0..10).map(|i| if i == 4 { None } else { Some(1.2) }).collect(),
            },
        ];
        render_trend(&path, "mega-evm 10-commit trend", &commits, &series).unwrap();
        assert_png(&path);
    }
}
