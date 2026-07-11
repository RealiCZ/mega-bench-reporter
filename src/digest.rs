//! Trend digest: rolls the last `digest_batch_size` commit records into
//! `digests/<day>-<range>/{summary.json, trend.png}`, plus the manual `trend`
//! subcommand's ad-hoc windows under `trends/`.

use crate::charts::{self, TrendSeries};
use crate::config::star_pattern_matches;
use crate::lane::Lane;
use crate::storage::{short_sha, CommitRecord, RepoStore, RowRecord};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The trend chart stays readable with a bounded series count; the full data
/// always lands in `summary.json`.
const TREND_MAX_SERIES: usize = 8;

/// One headline row's ratio series across the digest window, `summary.json`'s
/// `rows[]` entry — "table-ready": first/last/median precomputed.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SummaryRow {
    /// Full row key, e.g. `salt_dynamic_gas/rex5_salt/sstore_100`.
    pub row_key: String,
    /// `ratio_vs_baseline` per commit of the window (same order as
    /// `commits`); `null` where the row was missing that run.
    pub ratios: Vec<Option<f64>>,
    pub first: Option<f64>,
    pub last: Option<f64>,
    pub median: Option<f64>,
}

/// `summary.json` — the digest's structured source of truth.
#[derive(Debug, Clone, Serialize)]
pub struct DigestSummary {
    /// Full shas, oldest first.
    pub commits: Vec<String>,
    pub first_commit: String,
    pub last_commit: String,
    /// Headline-spec rows only, sorted by median ratio descending.
    pub rows: Vec<SummaryRow>,
    /// Union of the window's failed bench targets (deduped, sorted).
    pub failed_targets: Vec<String>,
    /// The instructions lane's counterpart to `rows`: same structure, each
    /// `ratios` value the commit's instructions `ratio_vs_baseline` (`null`
    /// for commits without instructions data). `None` — omitted from the JSON,
    /// keeping lane-off digests byte-identical — when no window commit carries
    /// any instructions ratio for a headline row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instr_series: Option<Vec<SummaryRow>>,
}

fn median_of(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("ratios are finite"));
    let n = sorted.len();
    Some(if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 })
}

/// One lane's headline-row series across the window: `row_key -> per-commit
/// value` (null-padded to the window length, aligned to `records`' order),
/// then table-ready `SummaryRow`s sorted by median descending. `value_of`
/// pulls the lane's scalar out of a stored row — `ratio_vs_baseline` for the
/// walltime series, the nested instructions ratio for the instructions one. A
/// row that never has a value in the window does not appear.
fn collect_series(
    records: &[CommitRecord],
    is_headline: &impl Fn(&str) -> bool,
    value_of: impl Fn(&RowRecord) -> Option<f64>,
) -> Vec<SummaryRow> {
    let mut series: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    for (i, record) in records.iter().enumerate() {
        for (group, rows) in &record.groups {
            for (row_name, row) in rows {
                let subject = row_name.split('/').next().unwrap_or(row_name);
                if !is_headline(subject) {
                    continue;
                }
                let Some(value) = value_of(row) else { continue };
                series
                    .entry(format!("{group}/{row_name}"))
                    .or_insert_with(|| vec![None; records.len()])[i] = Some(value);
            }
        }
    }

    let mut rows: Vec<SummaryRow> = series
        .into_iter()
        .map(|(row_key, ratios)| {
            let present: Vec<f64> = ratios.iter().flatten().copied().collect();
            SummaryRow {
                row_key,
                first: ratios.iter().flatten().next().copied(),
                last: ratios.iter().flatten().next_back().copied(),
                median: median_of(&present),
                ratios,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.median
            .partial_cmp(&a.median)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.row_key.cmp(&b.row_key))
    });
    rows
}

/// Extracts the headline rows' ratio series from the window's records — the
/// walltime `rows` plus, when any commit carries instructions data for a
/// headline row, the instructions `instr_series`.
pub fn build_summary(
    records: &[CommitRecord],
    is_headline: impl Fn(&str) -> bool,
) -> DigestSummary {
    let commits: Vec<String> = records.iter().map(|r| r.commit.clone()).collect();

    let rows = collect_series(records, &is_headline, |row| row.ratio_vs_baseline);
    let instr_rows = collect_series(records, &is_headline, |row| {
        row.instr.as_ref().and_then(|i| i.ratio_vs_baseline)
    });
    // Present only when the lane produced at least one non-null headline point
    // in the window — otherwise omitted so lane-off digests stay byte-stable.
    let instr_series = (!instr_rows.is_empty()).then_some(instr_rows);

    let mut failed: Vec<String> =
        records.iter().flat_map(|r| r.failed_targets.iter().cloned()).collect();
    failed.sort();
    failed.dedup();

    DigestSummary {
        first_commit: commits.first().cloned().unwrap_or_default(),
        last_commit: commits.last().cloned().unwrap_or_default(),
        commits,
        rows,
        failed_targets: failed,
        instr_series,
    }
}

/// Marks the points of a ratio series that sit more than `threshold_pct`
/// above the median of the points before them — the trend chart's red rings.
/// A window-local approximation of the live check (which uses the rolling
/// state), good enough for a visual cue.
fn alert_markers(ratios: &[Option<f64>], threshold_pct: f64) -> Vec<bool> {
    let mut seen: Vec<f64> = Vec::new();
    ratios
        .iter()
        .map(|r| {
            let Some(r) = *r else { return false };
            let alert = match median_of(&seen) {
                Some(m) if m > 0.0 => (r - m) / m * 100.0 > threshold_pct,
                _ => false,
            };
            if !alert {
                seen.push(r);
            }
            alert
        })
        .collect()
}

pub struct DigestOutcome {
    pub dir: PathBuf,
}

/// Writes a window's `summary.json` into `dir` (created if needed) — shared by
/// the automatic digest and the manual `trend` subcommand.
fn write_summary_json(dir: &Path, summary: &DigestSummary) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("summary.json"), serde_json::to_string_pretty(summary)?)?;
    Ok(())
}

/// Renders one lane's trend PNG from its `SummaryRow`s: the biggest-overhead
/// rows (already sorted by median descending), capped so the chart stays
/// readable, with red rings on the points that tripped `threshold_pct` vs the
/// window's prior median. `y_desc` names the value axis for the lane.
fn render_trend_png(
    path: &Path,
    title: &str,
    y_desc: &str,
    records: &[CommitRecord],
    rows: &[SummaryRow],
    threshold_pct: f64,
) -> anyhow::Result<()> {
    let commit_labels: Vec<String> = records.iter().map(|r| r.short_sha().to_string()).collect();
    let trend_series: Vec<TrendSeries> = rows
        .iter()
        .take(TREND_MAX_SERIES)
        .map(|row| TrendSeries {
            label: row.row_key.clone(),
            ratios: row.ratios.clone(),
            alerts: alert_markers(&row.ratios, threshold_pct),
        })
        .collect();
    charts::render_trend(path, title, y_desc, &commit_labels, &trend_series)
}

/// The walltime trend chart's y-axis label for `baseline`.
fn walltime_y_desc(baseline: &str) -> String {
    format!("time ratio vs {baseline} — lower is better")
}

/// The instructions trend chart's y-axis label for `baseline`.
fn instr_y_desc(baseline: &str) -> String {
    format!("instruction ratio vs {baseline} — lower is better")
}

/// Builds the digest directory (`summary.json` + `trend.png`, plus
/// `instr_trend.png` when the window carries instructions data) from the
/// window's records (oldest first). Pure data — the consumer composes its own
/// report from `summary.json` and the charts.
pub fn build_digest(
    store: &RepoStore,
    repo_name: &str,
    headline_label: &str,
    is_headline: impl Fn(&str) -> bool,
    regression_threshold_pct: f64,
    instr_regression_threshold_pct: f64,
    records: &[CommitRecord],
) -> anyhow::Result<DigestOutcome> {
    if records.is_empty() {
        anyhow::bail!("digest requested with no commit records");
    }
    let summary = build_summary(records, is_headline);
    if summary.rows.is_empty() {
        anyhow::bail!(
            "digest window has no '{headline_label}' rows with a baseline ratio — \
             nothing to summarize"
        );
    }

    let last = records.last().expect("non-empty");
    let dir = store.digest_dir(&last.day()?, &summary.first_commit, &summary.last_commit);
    let title = format!("{repo_name} headline ({headline_label}) — last {} commits", records.len());
    let baseline = &last.baseline_subject;
    write_summary_json(&dir, &summary)?;
    render_trend_png(
        &dir.join("trend.png"),
        &title,
        &walltime_y_desc(baseline),
        records,
        &summary.rows,
        regression_threshold_pct,
    )?;
    // The instructions trend rides alongside whenever the window has any
    // instructions data — same chart style, instructions thresholds for the
    // red rings, `null` commits drawn as gaps.
    if let Some(instr_rows) = &summary.instr_series {
        render_trend_png(
            &dir.join("instr_trend.png"),
            &format!("{title} — instructions"),
            &instr_y_desc(baseline),
            records,
            instr_rows,
            instr_regression_threshold_pct,
        )?;
    }
    Ok(DigestOutcome { dir })
}

/// Cuts a window out of the stored records (oldest first): `from`/`to` are
/// inclusive sha prefixes and take precedence; otherwise the most recent
/// `last` records.
pub fn select_window(
    mut records: Vec<CommitRecord>,
    last: usize,
    from: Option<&str>,
    to: Option<&str>,
) -> anyhow::Result<Vec<CommitRecord>> {
    let find = |prefix: &str| -> anyhow::Result<usize> {
        records
            .iter()
            .position(|r| r.commit.starts_with(prefix))
            .ok_or_else(|| anyhow::anyhow!("no stored commit matches '{prefix}'"))
    };
    if from.is_none() && to.is_none() {
        if last == 0 {
            anyhow::bail!("--last must be >= 1");
        }
        let skip = records.len().saturating_sub(last);
        return Ok(records.split_off(skip));
    }
    let lo = from.map(&find).transpose()?.unwrap_or(0);
    let hi = to.map(&find).transpose()?.unwrap_or_else(|| records.len().saturating_sub(1));
    if lo > hi {
        anyhow::bail!("--from is newer than --to (the window runs oldest to newest)");
    }
    Ok(records.drain(lo..=hi).collect())
}

#[derive(Debug)]
pub struct TrendOutcome {
    pub dir: PathBuf,
    /// Row keys that made it into `summary.json`, biggest median first.
    pub rows: Vec<String>,
    /// Full shas of the window, oldest first.
    pub commits: Vec<String>,
}

/// Row/output selection for [`build_adhoc_trend`].
pub struct TrendRequest<'a> {
    /// Row keys to chart (exact or trailing `*`); empty = the headline family.
    pub row_patterns: &'a [String],
    /// Explicit output directory; `None` = `trends/<day>-<first>..<last>`.
    pub out: Option<PathBuf>,
}

/// The manual counterpart of the digest: charts an arbitrary window of
/// already-stored records into `trends/` (or `request.out`). Read-only — no
/// bench, no state, no events, no digest counter. `metric` picks the lane:
/// `Walltime` renders `trend.png` (errors when the window has no walltime
/// headline ratios), `Instructions` renders `instr_trend.png` (errors when the
/// window has no instructions data). `summary.json` carries both series
/// regardless, so the JSON is the full picture either way.
#[allow(clippy::too_many_arguments)]
pub fn build_adhoc_trend(
    store: &RepoStore,
    repo_name: &str,
    headline_label: &str,
    is_headline: impl Fn(&str) -> bool,
    regression_threshold_pct: f64,
    instr_regression_threshold_pct: f64,
    metric: Lane,
    records: &[CommitRecord],
    request: TrendRequest<'_>,
) -> anyhow::Result<TrendOutcome> {
    let TrendRequest { row_patterns, out } = request;
    if records.is_empty() {
        anyhow::bail!("no stored commit records in the requested window");
    }
    // `--row` patterns widen the scan to every row, then narrow by row key;
    // the default scope is the configured headline family.
    let (mut summary, scope) = if row_patterns.is_empty() {
        (build_summary(records, is_headline), headline_label.to_string())
    } else {
        (build_summary(records, |_| true), row_patterns.join(", "))
    };
    if !row_patterns.is_empty() {
        let matches =
            |r: &SummaryRow| row_patterns.iter().any(|p| star_pattern_matches(p, &r.row_key));
        summary.rows.retain(matches);
        if let Some(instr) = summary.instr_series.as_mut() {
            instr.retain(matches);
        }
    }
    // A row filter that emptied the instructions series drops it entirely, so
    // the `Instructions` metric's empty-window check below fires correctly.
    if summary.instr_series.as_ref().is_some_and(|s| s.is_empty()) {
        summary.instr_series = None;
    }

    let last = records.last().expect("non-empty");
    let dir = match out {
        Some(dir) => dir,
        None => store.trend_dir(&last.day()?, &summary.first_commit, &summary.last_commit),
    };
    let baseline = &last.baseline_subject;
    let first7 = short_sha(&summary.first_commit).to_string();
    let last7 = short_sha(&summary.last_commit).to_string();
    let n = records.len();

    // The chosen lane drives the empty-window error, which chart is drawn, and
    // the returned row list; `summary.json` (both series) is written either way.
    let chart_rows: Vec<String> = match metric {
        Lane::Walltime => {
            if summary.rows.is_empty() {
                anyhow::bail!("no '{scope}' rows with a baseline ratio in the requested window");
            }
            write_summary_json(&dir, &summary)?;
            let title = format!("{repo_name} ({scope}) — {first7}..{last7} ({n} commits)");
            render_trend_png(
                &dir.join("trend.png"),
                &title,
                &walltime_y_desc(baseline),
                records,
                &summary.rows,
                regression_threshold_pct,
            )?;
            summary.rows.iter().map(|r| r.row_key.clone()).collect()
        }
        Lane::Instructions => {
            let Some(instr_rows) = summary.instr_series.as_ref() else {
                anyhow::bail!(
                    "no '{scope}' rows with an instructions ratio in the requested window — \
                     is the instructions lane enabled for these commits?"
                );
            };
            write_summary_json(&dir, &summary)?;
            let title =
                format!("{repo_name} ({scope}) — instructions — {first7}..{last7} ({n} commits)");
            render_trend_png(
                &dir.join("instr_trend.png"),
                &title,
                &instr_y_desc(baseline),
                records,
                instr_rows,
                instr_regression_threshold_pct,
            )?;
            instr_rows.iter().map(|r| r.row_key.clone()).collect()
        }
    };

    Ok(TrendOutcome { dir, rows: chart_rows, commits: summary.commits.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{InstrRecord, RowRecord};

    fn record_with(
        sha: &str,
        date: &str,
        rows: &[(&str, &str, f64, Option<f64>)],
        failed: &[&str],
    ) -> CommitRecord {
        let mut record = CommitRecord::new(
            sha.to_string(),
            date.to_string(),
            "rustc 1.86.0".into(),
            "revm_pinned".into(),
        );
        record.failed_targets = failed.iter().map(|s| s.to_string()).collect();
        for (group, row_name, ns, ratio) in rows {
            record.groups.entry(group.to_string()).or_default().insert(
                row_name.to_string(),
                RowRecord { ns: *ns, ratio_vs_baseline: *ratio, instr: None },
            );
        }
        record
    }

    fn window() -> Vec<CommitRecord> {
        (0..10)
            .map(|i| {
                record_with(
                    &format!("{:040x}", 0x1000 + i),
                    &format!("2026-07-0{}T0{}:00:00Z", i / 5 + 1, i % 5),
                    &[
                        (
                            "salt_dynamic_gas",
                            "rex5_salt/sstore_100",
                            28000.0,
                            Some(2.0 + i as f64 * 0.01),
                        ),
                        ("salt_dynamic_gas", "revm_pinned/sstore_100", 14000.0, Some(1.0)),
                        ("salt_dynamic_gas", "rex4/sstore_100", 20000.0, Some(1.43)),
                        ("empty_transaction", "rex5", 9000.0, Some(1.2)),
                        // A rexless row and a ratio-less rex5 row: both excluded.
                        ("oracle_real_data", "rex5_oracle/oracle_sload_50", 5000.0, None),
                    ],
                    if i == 3 { &["block_bench"] } else { &[] },
                )
            })
            .collect()
    }

    fn rex5_family(subject: &str) -> bool {
        subject == "rex5" || subject.starts_with("rex5_")
    }

    /// Like [`window`], but the `rex5_salt` headline row also carries
    /// instructions ratios on even-indexed commits — odd commits have no
    /// instructions data, so their positions must fall out as `null` in the
    /// instructions series (the null-padding contract).
    fn window_with_instr() -> Vec<CommitRecord> {
        let mut records = window();
        for (i, record) in records.iter_mut().enumerate() {
            if i % 2 != 0 {
                continue;
            }
            let salt = record
                .groups
                .get_mut("salt_dynamic_gas")
                .unwrap()
                .get_mut("rex5_salt/sstore_100")
                .unwrap();
            salt.instr = Some(InstrRecord { count: 25_000, ratio_vs_baseline: Some(2.5) });
        }
        records
    }

    #[test]
    fn test_build_summary_headline_rows_sorted_by_median() {
        let summary = build_summary(&window(), rex5_family);
        let keys: Vec<&str> = summary.rows.iter().map(|r| r.row_key.as_str()).collect();
        // rex4 and revm_pinned rows excluded; ratio-less rex5_oracle row excluded.
        assert_eq!(keys, vec!["salt_dynamic_gas/rex5_salt/sstore_100", "empty_transaction/rex5"]);
        let salt = &summary.rows[0];
        assert_eq!(salt.first, Some(2.0));
        assert_eq!(salt.last, Some(2.09));
        assert_eq!(salt.ratios.len(), 10);
        assert_eq!(summary.failed_targets, vec!["block_bench".to_string()]);
    }

    #[test]
    fn test_select_window_last_and_sha_bounds() {
        let records = window();
        let sha = |i: u64| format!("{:040x}", 0x1000 + i);

        let recent = select_window(records.clone(), 3, None, None).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].commit, sha(7));

        let mid = select_window(records.clone(), 999, Some(&sha(2)), Some(&sha(5))).unwrap();
        assert_eq!(mid.len(), 4);
        assert_eq!(mid[0].commit, sha(2));
        assert_eq!(mid[3].commit, sha(5));

        let from_only = select_window(records.clone(), 999, Some(&sha(8)), None).unwrap();
        assert_eq!(from_only.len(), 2);

        assert!(select_window(records.clone(), 5, Some("deadbeef"), None).is_err());
        assert!(select_window(records.clone(), 5, Some(&sha(5)), Some(&sha(2))).is_err());
        // last = 0 is a user error, not an implicit 1.
        assert!(select_window(records.clone(), 0, None, None).is_err());
        // ...but it is ignored (like any other last) when sha bounds are given.
        assert!(select_window(records, 0, Some(&sha(2)), Some(&sha(5))).is_ok());
    }

    #[test]
    fn test_build_adhoc_trend_row_filter_and_default_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        let records = window();

        // A non-headline row is reachable via an explicit --row pattern.
        let rex4 = build_adhoc_trend(
            &store,
            "mega-evm",
            "rex5, rex5_*",
            rex5_family,
            10.0,
            2.0,
            Lane::Walltime,
            &records,
            TrendRequest { row_patterns: &["salt_dynamic_gas/rex4/*".to_string()], out: None },
        )
        .unwrap();
        assert_eq!(rex4.rows, vec!["salt_dynamic_gas/rex4/sstore_100".to_string()]);
        assert_eq!(rex4.commits.len(), 10);
        assert!(rex4.dir.starts_with(tmp.path().join("mega-evm").join("trends")));
        assert!(rex4.dir.join("summary.json").is_file());
        assert!(rex4.dir.join("trend.png").is_file());

        // Default scope = the headline family.
        let headline = build_adhoc_trend(
            &store,
            "mega-evm",
            "rex5, rex5_*",
            rex5_family,
            10.0,
            2.0,
            Lane::Walltime,
            &records,
            TrendRequest { row_patterns: &[], out: None },
        )
        .unwrap();
        assert_eq!(headline.rows.len(), 2);

        // A pattern matching nothing is an error, not an empty chart.
        assert!(build_adhoc_trend(
            &store,
            "mega-evm",
            "rex5, rex5_*",
            rex5_family,
            10.0,
            2.0,
            Lane::Walltime,
            &records,
            TrendRequest { row_patterns: &["nope/*".to_string()], out: None },
        )
        .is_err());
    }

    #[test]
    fn test_build_summary_instr_series_null_padding_and_omission() {
        // Lane off (no instr anywhere): instr_series is None and omitted from
        // the serialized summary, keeping lane-off digests byte-stable.
        let summary = build_summary(&window(), rex5_family);
        assert!(summary.instr_series.is_none());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("instr_series"), "lane-off summary must not mention it: {json}");

        // Lane on for even commits only: the instructions series carries the
        // salt row, null-padded at the odd commits with no instructions data.
        let summary = build_summary(&window_with_instr(), rex5_family);
        let instr = summary.instr_series.expect("instr_series present");
        let salt = instr
            .iter()
            .find(|r| r.row_key == "salt_dynamic_gas/rex5_salt/sstore_100")
            .expect("salt row in instr_series");
        assert_eq!(salt.ratios.len(), 10, "aligned to the same commits as rows");
        for (i, ratio) in salt.ratios.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(*ratio, Some(2.5), "commit {i} has instructions data");
            } else {
                assert_eq!(*ratio, None, "commit {i} has no instructions data → null");
            }
        }
        // The non-instrumented rex5 headline row is absent from instr_series.
        assert!(instr.iter().all(|r| r.row_key != "empty_transaction/rex5"));
    }

    #[test]
    fn test_build_digest_renders_instr_trend_only_with_data() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");

        // Lane off: trend.png only, no instr_trend.png, no instr_series key.
        let off =
            build_digest(&store, "mega-evm", "rex5, rex5_*", rex5_family, 10.0, 2.0, &window())
                .unwrap();
        assert!(off.dir.join("trend.png").is_file());
        assert!(!off.dir.join("instr_trend.png").exists());
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(off.dir.join("summary.json")).unwrap())
                .unwrap();
        assert!(summary.get("instr_series").is_none());

        // Lane on: both charts, and summary.json carries instr_series.
        let on = build_digest(
            &store,
            "mega-evm",
            "rex5, rex5_*",
            rex5_family,
            10.0,
            2.0,
            &window_with_instr(),
        )
        .unwrap();
        assert!(on.dir.join("trend.png").is_file());
        assert!(on.dir.join("instr_trend.png").is_file());
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(on.dir.join("summary.json")).unwrap())
                .unwrap();
        assert!(summary["instr_series"].is_array());
    }

    #[test]
    fn test_build_adhoc_trend_metric_instructions_happy_and_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");

        // Happy path: instructions data present → instr_trend.png + a
        // summary.json carrying instr_series; only the chosen lane's chart is
        // drawn (no trend.png), and the returned rows are the instr series.
        let ok = build_adhoc_trend(
            &store,
            "mega-evm",
            "rex5, rex5_*",
            rex5_family,
            10.0,
            2.0,
            Lane::Instructions,
            &window_with_instr(),
            TrendRequest { row_patterns: &[], out: None },
        )
        .unwrap();
        assert!(ok.dir.join("instr_trend.png").is_file());
        assert!(!ok.dir.join("trend.png").exists());
        assert!(ok.dir.join("summary.json").is_file());
        assert_eq!(ok.rows, vec!["salt_dynamic_gas/rex5_salt/sstore_100".to_string()]);
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(ok.dir.join("summary.json")).unwrap())
                .unwrap();
        assert!(summary["instr_series"].is_array());

        // Empty window: no instructions data anywhere → an actionable error,
        // not an empty chart.
        let err = build_adhoc_trend(
            &store,
            "mega-evm",
            "rex5, rex5_*",
            rex5_family,
            10.0,
            2.0,
            Lane::Instructions,
            &window(),
            TrendRequest { row_patterns: &[], out: None },
        );
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("instructions"));
    }

    #[test]
    fn test_build_digest_writes_summary_and_trend() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        let records = window();
        let outcome =
            build_digest(&store, "mega-evm", "rex5, rex5_*", rex5_family, 10.0, 2.0, &records)
                .unwrap();

        // Directory named after the last commit's day + the sha range.
        assert_eq!(
            outcome.dir,
            store.digest_dir("20260702", &records[0].commit, &records[9].commit)
        );
        assert!(outcome.dir.join("summary.json").is_file());
        assert!(outcome.dir.join("trend.png").is_file());
        let summary: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(outcome.dir.join("summary.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(summary["failed_targets"][0], "block_bench");
    }

    #[test]
    fn test_build_digest_without_headline_rows_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        let records = vec![record_with(
            &format!("{:040x}", 0x99),
            "2026-07-02T10:00:00Z",
            &[("g", "revm_pinned/w", 1.0, Some(1.0))],
            &[],
        )];
        assert!(build_digest(&store, "r", "rex5", rex5_family, 10.0, 2.0, &records).is_err());
    }
}
