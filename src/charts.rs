//! Chart rendering with `plotters`: relative-speed bars, violin
//! (distribution) plots, and the digest trend chart — all PNG, all using an
//! embedded font so the binary has no system font/fontconfig dependency.
//! Derived data artifacts (e.g. the comparison table) live in
//! [`crate::compare`]; this module only draws.

use crate::criterion_results::Row;
use plotters::coord::types::RangedCoordf64;
use plotters::prelude::*;
use plotters::style::register_font;
use plotters::style::text_anchor::{HPos, Pos, VPos};
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

/// 8-slot categorical palette, ordering chosen for color-vision-deficiency
/// separation (worst adjacent-pair ΔE 24.2 on a white surface). Deliberately
/// distinct from [`ALERT_RED`] so a series line never impersonates the
/// regression marker.
const PALETTE: &[RGBColor] = &[
    RGBColor(0x2a, 0x78, 0xd6), // blue
    RGBColor(0x1b, 0xaf, 0x7a), // aqua
    RGBColor(0xed, 0xa1, 0x00), // yellow
    RGBColor(0x00, 0x83, 0x00), // green
    RGBColor(0x4a, 0x3a, 0xa7), // violet
    RGBColor(0xe3, 0x49, 0x48), // red
    RGBColor(0xe8, 0x7b, 0xa4), // magenta
    RGBColor(0xeb, 0x68, 0x34), // orange
];

/// Status color for regression markers — reserved, never a series color.
const ALERT_RED: RGBColor = RGBColor(0xd0, 0x3b, 0x3b);

/// All charts render at 2× and are displayed downscaled: plotters' bitmap
/// text has no hinting/subpixel positioning, so small glyphs come out ragged
/// at 1:1 — supersampling is what keeps text crisp in chat/browser embeds.
const SS: i32 = 2;

/// Palette slot → color. Slots past the base palette reuse its hues as
/// darkened, then lightened shades, so a run with more series than palette
/// entries (mega-evm's full comparison set is 11 subjects) never paints two
/// of them identically.
fn series_color(i: usize) -> RGBColor {
    let RGBColor(r, g, b) = PALETTE[i % PALETTE.len()];
    let scale = |c: u8, num: u16, den: u16, add: u16| ((c as u16 * num / den) + add) as u8;
    match (i / PALETTE.len()) % 3 {
        0 => RGBColor(r, g, b),
        1 => RGBColor(scale(r, 3, 5, 0), scale(g, 3, 5, 0), scale(b, 3, 5, 0)),
        _ => RGBColor(scale(r, 1, 2, 128), scale(g, 1, 2, 128), scale(b, 1, 2, 128)),
    }
}

/// Neutral color reserved for the baseline subject in subject-keyed charts —
/// the reference everything is measured against, visually de-emphasized so
/// palette colors always mean "an implementation under comparison".
const BASELINE_GRAY: RGBColor = RGBColor(144, 153, 176);

/// Stable subject→color assignment shared by every subject-keyed chart of a
/// run (speed bars and violins): the baseline gets [`BASELINE_GRAY`]; the
/// priority (headline) subjects take the leading bright palette slots in
/// alphabetical order, the remaining subjects follow alphabetically. Built
/// once per run from all parsed rows, so `rex5_salt` wears the same color in
/// the speed bars and in every violin even though each chart shows a
/// different subject subset, and stable across commits for a stable subject +
/// headline set. (The trend chart is keyed on rows, not subjects — many rows
/// share a subject — so it keeps its own per-row-key scheme.)
#[derive(Debug, Clone)]
pub struct SubjectColors {
    baseline: String,
    slots: std::collections::BTreeMap<String, usize>,
}

impl SubjectColors {
    pub fn new<I, S>(
        baseline_subject: &str,
        subjects: I,
        is_priority: impl Fn(&str) -> bool,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut non_baseline: Vec<String> = subjects
            .into_iter()
            .map(Into::into)
            .filter(|s| s != baseline_subject)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        // Headline subjects are what the speed bars show — they get the
        // bright leading slots; shade-variant slots (if the set outgrows the
        // palette) land on the comparison-only subjects.
        non_baseline.sort_by_key(|s| (!is_priority(s), s.clone()));
        let slots = non_baseline.into_iter().enumerate().map(|(i, s)| (s, i)).collect();
        Self { baseline: baseline_subject.to_string(), slots }
    }

    pub fn color(&self, subject: &str) -> RGBColor {
        if subject == self.baseline {
            BASELINE_GRAY
        } else if let Some(&slot) = self.slots.get(subject) {
            series_color(slot)
        } else {
            // A subject outside the set this mapping was built from —
            // deterministic, but callers should build from the full set.
            series_color(self.slots.len())
        }
    }

    pub fn baseline(&self) -> &str {
        &self.baseline
    }
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

/// One item of the speed bar chart: baseline plus each headline subject's
/// relative speed (`100 × baseline_time / subject_time`; baseline = 100%,
/// lower = more overhead).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedBarItem {
    pub item: String,
    /// `(subject, percent)`, baseline first.
    pub bars: Vec<(String, f64)>,
}

/// Grouped horizontal bar chart, one group per test item (horizontal because
/// real item names like `salt_dynamic_gas/sstore_100` are far too long for
/// the mock's vertical layout). The subject legend sits in its own panel to
/// the right of the plot, like the trend chart's, so it never competes with
/// bars for space.
pub fn render_speed_bars(
    path: &Path,
    title: &str,
    items: &[SpeedBarItem],
    colors: &SubjectColors,
) -> anyhow::Result<()> {
    ensure_font();
    if items.is_empty() {
        anyhow::bail!("no items to render speed bars for");
    }
    let baseline_subject = colors.baseline();
    // Legend order = subject order of first appearance (baseline first, the
    // way the pipeline feeds bars); also fixes each band's bar slot count.
    let mut legend_subjects: Vec<String> = Vec::new();
    for item in items {
        for (subject, _) in &item.bars {
            if !legend_subjects.contains(subject) {
                legend_subjects.push(subject.clone());
            }
        }
    }

    let band = legend_subjects.len().max(1) as f64 + 1.0; // bars + gap per item
    let total = items.len() as f64 * band - 1.0;
    let max_percent =
        items.iter().flat_map(|i| i.bars.iter().map(|(_, p)| *p)).fold(100.0_f64, f64::max);
    let x_max = (max_percent * 1.15).max(118.0);
    let ss = SS as u32;
    let height =
        (150 + items.len() * (legend_subjects.len() * 22 + 14)).clamp(300, 2200) as u32 * ss;
    // Legend panel outside the plot; width fits the longest subject name.
    let max_subject_chars = legend_subjects.iter().map(|s| s.len()).max().unwrap_or(0) as u32;
    let legend_w = (60 + max_subject_chars * 8).clamp(140, 320) * ss;
    let root = BitMapBackend::new(path, (1100 * ss + legend_w, height)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;
    let (plot_area, legend_area) = root.split_horizontally(1100 * ss);

    let labels: Vec<&str> = items.iter().map(|i| i.item.as_str()).collect();
    let mut chart = ChartBuilder::on(&plot_area)
        .caption(title, ("sans-serif", 22 * SS))
        .margin(15 * SS)
        .x_label_area_size(45 * SS)
        .y_label_area_size(340 * SS)
        .build_cartesian_2d(0.0..x_max, -0.6..(total + 0.6))
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
        .y_labels((total as usize) * 4 + 5)
        .y_label_formatter(&|v: &f64| {
            for (i, label) in labels.iter().enumerate() {
                if (v - band_center(i)).abs() <= 0.26 {
                    return label.to_string();
                }
            }
            String::new()
        })
        .x_desc(
            format!("relative speed, {baseline_subject} = 100% (lower = more overhead)").as_str(),
        )
        .label_style(("sans-serif", 14 * SS))
        .axis_desc_style(("sans-serif", 16 * SS))
        .draw()
        .map_err(map_err)?;

    for (i, item) in items.iter().enumerate() {
        let base = (items.len() - 1 - i) as f64 * band;
        for (b, (subject, percent)) in item.bars.iter().enumerate() {
            // Bar 0 (baseline) at the top of the band.
            let y = base + (legend_subjects.len() - 1 - b.min(legend_subjects.len() - 1)) as f64;
            let color = colors.color(subject);
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
                    ("sans-serif", 13 * SS).into_font().color(&BLACK),
                )))
                .map_err(map_err)?;
        }
    }

    // 100% reference line.
    chart
        .draw_series(LineSeries::new(
            [(100.0, -0.6), (100.0, total + 0.6)],
            BLACK.mix(0.45).stroke_width(ss),
        ))
        .map_err(map_err)?;

    // Legend panel: color swatch + subject name, one row per subject.
    let row_h = 24 * SS;
    let top = 52 * SS;
    for (pos, subject) in legend_subjects.iter().enumerate() {
        let color = colors.color(subject);
        let y = top + pos as i32 * row_h;
        legend_area
            .draw(&Rectangle::new(
                [(10 * SS, y - 5 * SS), (24 * SS, y + 5 * SS)],
                color.mix(0.9).filled(),
            ))
            .map_err(map_err)?;
        legend_area
            .draw(&Text::new(
                subject.clone(),
                (32 * SS, y - 7 * SS),
                ("sans-serif", 13 * SS).into_font().color(&BLACK.mix(0.75)),
            ))
            .map_err(map_err)?;
    }

    root.present().map_err(map_err)?;
    Ok(())
}

/// Gaussian KDE over `grid` with Silverman's rule-of-thumb bandwidth.
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
/// x = per-call time in µs, with min–max whiskers and a mean tick. Violin
/// colors come from the run-wide [`SubjectColors`] so they agree with the
/// speed-bar chart.
pub fn render_violin(
    path: &Path,
    title: &str,
    rows: &[&Row],
    colors: &SubjectColors,
) -> anyhow::Result<()> {
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

    let ss = SS as u32;
    let height = (120 + n * 110).clamp(260, 1200) as u32 * ss;
    let root = BitMapBackend::new(path, (1100 * ss, height)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;

    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("sans-serif", 22 * SS))
        .margin(15 * SS)
        .x_label_area_size(45 * SS)
        .y_label_area_size(330 * SS)
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
        .label_style(("sans-serif", 15 * SS))
        .axis_desc_style(("sans-serif", 16 * SS))
        .draw()
        .map_err(map_err)?;

    let y_of = |i: usize| (n - 1 - i) as f64;
    for (i, samples) in samples_us.iter().enumerate() {
        let color = colors.color(&rows[i].subject);
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
            .draw_series(LineSeries::new([(lo, y), (hi, y)], color.stroke_width(ss)))
            .map_err(map_err)?;
        chart
            .draw_series(LineSeries::new(
                [(mean, y - half * 0.7), (mean, y + half * 0.7)],
                color.stroke_width(2 * ss),
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
/// x = commit (short sha), y = `× vs baseline`.
pub fn render_trend(
    path: &Path,
    title: &str,
    baseline_subject: &str,
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
    // Tight y-range around the data (with a minimum span so a flat window
    // doesn't zoom into pure noise); the 1.0× parity line is drawn only when
    // it falls inside the range instead of forcing the range to include it.
    let y_min = all.iter().copied().fold(f64::INFINITY, f64::min);
    let y_max = all.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (y_max - y_min).max(y_max.abs() * 0.04).max(1e-9);
    let pad = span * 0.10;
    let (y_lo, y_hi) = (y_min - pad, y_max + pad * 1.6);

    // Color follows the row, not its overhead rank: series arrive sorted by
    // median, so a palette slot keyed on input order would repaint a row
    // whenever its rank shifts between digests. Alphabetical rank is stable
    // for a stable headline set.
    let mut alpha_order: Vec<usize> = (0..series.len()).collect();
    alpha_order.sort_by(|&a, &b| series[a].label.cmp(&series[b].label));
    let mut palette_slot = vec![0usize; series.len()];
    for (slot, &idx) in alpha_order.iter().enumerate() {
        palette_slot[idx] = slot;
    }

    // Legend entries sorted by last value descending so the panel matches the
    // vertical order of the line endpoints.
    let last_value = |s: &TrendSeries| s.ratios.iter().rev().flatten().next().copied();
    let mut legend_order: Vec<usize> = (0..series.len()).collect();
    legend_order.sort_by(|&a, &b| {
        last_value(&series[b])
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&last_value(&series[a]).unwrap_or(f64::NEG_INFINITY))
    });

    let ss = SS as u32;

    // Legend panel sits outside the plot; width fits the longest label.
    let max_label_chars = series.iter().map(|s| s.label.len()).max().unwrap_or(0) as u32;
    let legend_w = (110 + max_label_chars * 7).clamp(220, 460) * ss;
    let (width, height) = (960 * ss + legend_w, 520 * ss);

    let n = commit_labels.len();
    let label_refs: Vec<&str> = commit_labels.iter().map(|s| s.as_str()).collect();
    let root = BitMapBackend::new(path, (width, height)).into_drawing_area();
    root.fill(&WHITE).map_err(map_err)?;
    let (plot_area, legend_area) = root.split_horizontally(width - legend_w);

    let mut chart = ChartBuilder::on(&plot_area)
        .caption(title, ("sans-serif", 20 * SS))
        .margin(12 * SS)
        .x_label_area_size(50 * SS)
        .y_label_area_size(70 * SS)
        .build_cartesian_2d(-0.5..(n as f64 - 0.5), y_lo..y_hi)
        .map_err(map_err)?;

    chart
        .configure_mesh()
        .light_line_style(TRANSPARENT)
        .bold_line_style(BLACK.mix(0.08))
        .y_labels(8)
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
        .y_desc(format!("time ratio vs {baseline_subject} — lower is better").as_str())
        .label_style(("sans-serif", 13 * SS))
        .axis_desc_style(("sans-serif", 15 * SS))
        .draw()
        .map_err(map_err)?;

    if (y_lo..y_hi).contains(&1.0) {
        chart
            .draw_series(LineSeries::new(
                [(-0.5, 1.0), (n as f64 - 0.5, 1.0)],
                BLACK.mix(0.4).stroke_width(SS as u32),
            ))
            .map_err(map_err)?;
    }

    // Marker shape is the second identity channel next to color (several
    // palette slots are close under color-vision deficiency).
    fn draw_marker<DB: DrawingBackend>(
        chart: &mut ChartContext<'_, DB, Cartesian2d<RangedCoordf64, RangedCoordf64>>,
        shape: usize,
        pt: (f64, f64),
        color: RGBColor,
    ) -> Result<(), anyhow::Error>
    where
        DB::ErrorType: 'static,
    {
        match shape % 4 {
            0 => chart
                .draw_series(std::iter::once(Circle::new(pt, 4 * SS, color.filled())))
                .map(|_| ()),
            1 => chart
                .draw_series(std::iter::once(TriangleMarker::new(pt, 5 * SS, color.filled())))
                .map(|_| ()),
            2 => chart
                .draw_series(std::iter::once(
                    EmptyElement::at(pt)
                        + Rectangle::new([(-4 * SS, -4 * SS), (4 * SS, 4 * SS)], color.filled()),
                ))
                .map(|_| ()),
            _ => chart
                .draw_series(std::iter::once(Cross::new(
                    pt,
                    4 * SS as u32,
                    color.stroke_width(2 * SS as u32),
                )))
                .map(|_| ()),
        }
        .map_err(map_err)
    }

    for (i, s) in series.iter().enumerate() {
        let color = series_color(palette_slot[i]);
        let points: Vec<(f64, f64)> =
            s.ratios.iter().enumerate().filter_map(|(x, r)| r.map(|r| (x as f64, r))).collect();
        chart
            .draw_series(LineSeries::new(points.clone(), color.stroke_width(2 * SS as u32)))
            .map_err(map_err)?;
        for &pt in &points {
            draw_marker(&mut chart, palette_slot[i], pt, color)?;
        }
        // Ring on the points that tripped the regression threshold.
        for (x, r) in s.ratios.iter().enumerate() {
            if s.alerts.get(x).copied().unwrap_or(false) {
                if let Some(r) = r {
                    chart
                        .draw_series(std::iter::once(Circle::new(
                            (x as f64, *r),
                            8 * SS,
                            ALERT_RED.stroke_width(2 * SS as u32),
                        )))
                        .map_err(map_err)?;
                }
            }
        }
    }

    // Legend panel: swatch (line + marker), last value, full row key.
    let row_h = 24 * SS;
    let top = 52 * SS;
    for (pos, &i) in legend_order.iter().enumerate() {
        let s = &series[i];
        let color = series_color(palette_slot[i]);
        let y = top + pos as i32 * row_h;
        legend_area
            .draw(&PathElement::new(
                [(10 * SS, y), (38 * SS, y)],
                color.stroke_width(2 * SS as u32),
            ))
            .map_err(map_err)?;
        let mid = (24 * SS, y);
        match palette_slot[i] % 4 {
            0 => legend_area.draw(&Circle::new(mid, 4 * SS, color.filled())),
            1 => legend_area.draw(&TriangleMarker::new(mid, 5 * SS, color.filled())),
            2 => legend_area.draw(&Rectangle::new(
                [(20 * SS, y - 4 * SS), (28 * SS, y + 4 * SS)],
                color.filled(),
            )),
            _ => {
                legend_area.draw(&Cross::new(mid, 4 * SS as u32, color.stroke_width(2 * SS as u32)))
            }
        }
        .map_err(map_err)?;
        // Right-anchored value column: a proportional font can't be aligned
        // with space padding.
        let value = last_value(s).map(|v| format!("{v:.2}×")).unwrap_or_else(|| "—".into());
        legend_area
            .draw(&Text::new(
                value,
                (92 * SS, y - 7 * SS),
                ("sans-serif", 13 * SS)
                    .into_font()
                    .color(&BLACK)
                    .pos(Pos::new(HPos::Right, VPos::Top)),
            ))
            .map_err(map_err)?;
        legend_area
            .draw(&Text::new(
                s.label.clone(),
                (100 * SS, y - 7 * SS),
                ("sans-serif", 13 * SS).into_font().color(&BLACK.mix(0.75)),
            ))
            .map_err(map_err)?;
    }
    // Explain the one status marker the chart can carry.
    let y = top + series.len() as i32 * row_h + 10 * SS;
    legend_area
        .draw(&Circle::new((24 * SS, y), 8 * SS, ALERT_RED.stroke_width(2 * SS as u32)))
        .map_err(map_err)?;
    legend_area
        .draw(&Text::new(
            "regression alert",
            (46 * SS, y - 7 * SS),
            ("sans-serif", 13 * SS).into_font().color(&BLACK.mix(0.75)),
        ))
        .map_err(map_err)?;

    root.present().map_err(map_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_png(path: &Path) {
        let bytes = std::fs::read(path).expect("chart file written");
        assert!(bytes.len() > 1000, "suspiciously small png ({} bytes)", bytes.len());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a png");
    }

    fn fake_samples(center_ns: f64, n: usize) -> Vec<f64> {
        // Deterministic pseudo-noise around the center; no rand dependency.
        (0..n).map(|i| center_ns * (1.0 + 0.02 * ((i as f64 * 2.399).sin()))).collect()
    }

    fn sample_colors() -> SubjectColors {
        SubjectColors::new(
            "revm_pinned",
            ["revm_pinned", "rex4", "rex5", "rex5_salt", "rex5_oracle", "s"],
            |s| s.starts_with("rex5"),
        )
    }

    #[test]
    fn test_subject_colors_baseline_reserved_and_headline_gets_leading_slots() {
        let colors = sample_colors();
        assert_eq!(colors.baseline(), "revm_pinned");
        assert_eq!(colors.color("revm_pinned"), BASELINE_GRAY);
        // Priority (headline) subjects take the leading bright slots in
        // alphabetical order; the rest follow — independent of which chart
        // asks.
        assert_eq!(colors.color("rex5"), PALETTE[0]);
        assert_eq!(colors.color("rex5_oracle"), PALETTE[1]);
        assert_eq!(colors.color("rex5_salt"), PALETTE[2]);
        assert_eq!(colors.color("rex4"), PALETTE[3]);
        assert_eq!(colors.color("s"), PALETTE[4]);
        // The baseline never occupies a palette slot.
        assert!(PALETTE.iter().all(|c| *c != BASELINE_GRAY));
    }

    #[test]
    fn test_subject_colors_stay_distinct_past_the_palette_size() {
        // mega-evm's real comparison set is 11 non-baseline subjects — more
        // than the 8 base palette slots. Slots past the palette must shade,
        // not repeat: every subject keeps a unique color.
        let subjects: Vec<String> = (0..12).map(|i| format!("subject_{i:02}")).collect();
        let colors = SubjectColors::new("base", subjects.iter().cloned(), |_| false);
        let mut seen = std::collections::BTreeSet::new();
        for s in &subjects {
            let RGBColor(r, g, b) = colors.color(s);
            assert!(seen.insert((r, g, b)), "duplicate color for {s}");
        }
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
        render_speed_bars(&path, "relative speed (revm_pinned = 100%)", &items, &sample_colors())
            .unwrap();
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
        render_violin(
            &path,
            "salt_dynamic_gas/sstore_100",
            &[&baseline, &feature],
            &sample_colors(),
        )
        .unwrap();
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
        render_violin(&path, "degenerate", &[&row], &sample_colors()).unwrap();
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
        render_trend(&path, "mega-evm 10-commit trend", "revm_pinned", &commits, &series).unwrap();
        assert_png(&path);
    }
}
