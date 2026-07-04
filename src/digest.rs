//! Trend digest: rolls the last `digest_batch_size`
//! commit records into `digests/<day>-<range>/{summary.json, trend.png}` plus
//! a ready-to-post trend-digest card.

use crate::charts::{self, TrendSeries};
use crate::storage::{short_sha, CommitRecord, RepoStore};
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

/// Extracts the headline rows' ratio series from the window's records.
pub fn build_summary(
    records: &[CommitRecord],
    is_headline: impl Fn(&str) -> bool,
) -> DigestSummary {
    let commits: Vec<String> = records.iter().map(|r| r.commit.clone()).collect();

    // row_key -> per-commit ratios.
    let mut series: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    for (i, record) in records.iter().enumerate() {
        for (group, rows) in &record.groups {
            for (row_name, row) in rows {
                let subject = row_name.split('/').next().unwrap_or(row_name);
                if !is_headline(subject) {
                    continue;
                }
                let Some(ratio) = row.ratio_vs_baseline else { continue };
                series
                    .entry(format!("{group}/{row_name}"))
                    .or_insert_with(|| vec![None; records.len()])[i] = Some(ratio);
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

/// Writes a window's `summary.json` + `trend.png` into `dir` — shared by the
/// automatic digest and the manual `trend` subcommand.
fn write_window_artifacts(
    dir: &Path,
    title: &str,
    regression_threshold_pct: f64,
    records: &[CommitRecord],
    summary: &DigestSummary,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("summary.json"), serde_json::to_string_pretty(summary)?)?;

    // Trend chart: the biggest-overhead rows (summary is sorted by median
    // descending), capped so the chart stays readable.
    let commit_labels: Vec<String> = records.iter().map(|r| r.short_sha().to_string()).collect();
    let trend_series: Vec<TrendSeries> = summary
        .rows
        .iter()
        .take(TREND_MAX_SERIES)
        .map(|row| TrendSeries {
            label: row.row_key.clone(),
            ratios: row.ratios.clone(),
            alerts: alert_markers(&row.ratios, regression_threshold_pct),
        })
        .collect();
    let baseline_subject = &records.last().expect("non-empty window").baseline_subject;
    charts::render_trend(
        &dir.join("trend.png"),
        title,
        baseline_subject,
        &commit_labels,
        &trend_series,
    )
}

/// Builds the digest directory (`summary.json` + `trend.png`) from the
/// window's records (oldest first). Pure data — the consumer composes its own
/// report from `summary.json` and the chart.
pub fn build_digest(
    store: &RepoStore,
    repo_name: &str,
    headline_label: &str,
    is_headline: impl Fn(&str) -> bool,
    regression_threshold_pct: f64,
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
    write_window_artifacts(&dir, &title, regression_threshold_pct, records, &summary)?;
    Ok(DigestOutcome { dir })
}

/// `true` when `key` matches `pattern` — exact, or prefix when the pattern
/// ends with `*`.
fn key_matches(pattern: &str, key: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => pattern == key,
    }
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
/// bench, no state, no events, no digest counter.
pub fn build_adhoc_trend(
    store: &RepoStore,
    repo_name: &str,
    headline_label: &str,
    is_headline: impl Fn(&str) -> bool,
    regression_threshold_pct: f64,
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
        summary.rows.retain(|r| row_patterns.iter().any(|p| key_matches(p, &r.row_key)));
    }
    if summary.rows.is_empty() {
        anyhow::bail!("no '{scope}' rows with a baseline ratio in the requested window");
    }

    let last = records.last().expect("non-empty");
    let dir = match out {
        Some(dir) => dir,
        None => store.trend_dir(&last.day()?, &summary.first_commit, &summary.last_commit),
    };
    let title = format!(
        "{repo_name} ({scope}) — {}..{} ({} commits)",
        short_sha(&summary.first_commit),
        short_sha(&summary.last_commit),
        records.len()
    );
    write_window_artifacts(&dir, &title, regression_threshold_pct, records, &summary)?;
    Ok(TrendOutcome {
        dir,
        rows: summary.rows.iter().map(|r| r.row_key.clone()).collect(),
        commits: summary.commits.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RowRecord;

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
            record
                .groups
                .entry(group.to_string())
                .or_default()
                .insert(row_name.to_string(), RowRecord { ns: *ns, ratio_vs_baseline: *ratio });
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
            &records,
            TrendRequest { row_patterns: &["nope/*".to_string()], out: None },
        )
        .is_err());
    }

    #[test]
    fn test_build_digest_writes_summary_and_trend() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        let records = window();
        let outcome =
            build_digest(&store, "mega-evm", "rex5, rex5_*", rex5_family, 10.0, &records).unwrap();

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
        assert!(build_digest(&store, "r", "rex5", rex5_family, 10.0, &records).is_err());
    }
}
