//! Categorized local storage under `<data-root>/<repo>/` (Task 1.3):
//!
//! ```text
//! <data-root>/<repo>/
//!   commits/<YYYYMMDD>-<shortsha>/{raw.json, compare.png, dist_*.png}
//!   digests/<YYYYMMDD>-<shortsha-range>/{summary.json, trend.png}
//!   flame/<YYYYMMDD>/{<workload>.svg, <workload>_diff.svg}
//!   state.json
//! ```
//!
//! `<repo>` is the outermost axis so a second tracked repo is a new top-level
//! directory, not a schema change. Structured JSON (`raw.json`) is the source
//! of truth; PNGs are derived artifacts.

use crate::criterion_results::WorkloadRatios;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// One row's stored numbers inside `raw.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RowRecord {
    /// Mean wall-clock per call, nanoseconds.
    pub ns: f64,
    /// `mean_ns / revm_pinned mean_ns` for the same `(group, workload)`;
    /// `None` when the group/workload has no `revm_pinned` baseline row.
    pub ratio_vs_revm_pinned: Option<f64>,
}

/// `raw.json` — the structured source of truth for one benched commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitRecord {
    /// Full commit sha.
    pub commit: String,
    /// Committer date, RFC3339 — also the sort key for "last N commits".
    pub date: String,
    /// `rustc --version` used for the run.
    pub rustc: String,
    /// Bench targets that failed to compile or run this commit. Marked, not
    /// silently dropped; their rows are simply absent from `groups`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_targets: Vec<String>,
    /// `group -> row ("subject" or "subject/workload") -> numbers`.
    pub groups: BTreeMap<String, BTreeMap<String, RowRecord>>,
}

impl CommitRecord {
    pub fn new(commit: String, date: String, rustc: String) -> Self {
        Self { commit, date, rustc, failed_targets: Vec::new(), groups: BTreeMap::new() }
    }

    /// Folds parsed ratio tables into `groups`.
    pub fn add_ratios(&mut self, ratios: &[WorkloadRatios]) {
        for wl in ratios {
            let group = self.groups.entry(wl.group.clone()).or_default();
            for row in &wl.rows {
                group.insert(
                    row_name(&row.subject, &wl.workload),
                    RowRecord { ns: row.mean_ns, ratio_vs_revm_pinned: row.ratio_vs_revm_pinned },
                );
            }
        }
    }

    pub fn short_sha(&self) -> &str {
        short_sha(&self.commit)
    }

    /// `YYYYMMDD` taken from `date`.
    pub fn day(&self) -> anyhow::Result<String> {
        day_of(&self.date)
    }
}

/// Row key inside a group: `subject/workload`, or just `subject` for bare rows.
pub fn row_name(subject: &str, workload: &str) -> String {
    if workload.is_empty() {
        subject.to_string()
    } else {
        format!("{subject}/{workload}")
    }
}

pub fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn day_of(rfc3339: &str) -> anyhow::Result<String> {
    let dt = OffsetDateTime::parse(rfc3339, &Rfc3339)
        .map_err(|e| anyhow::anyhow!("bad RFC3339 date '{rfc3339}': {e}"))?;
    Ok(format!("{:04}{:02}{:02}", dt.year(), dt.month() as u8, dt.day()))
}

/// All paths for one tracked repo under the data root.
#[derive(Debug, Clone)]
pub struct RepoStore {
    root: PathBuf,
}

impl RepoStore {
    pub fn new(data_root: &Path, repo_name: &str) -> Self {
        Self { root: data_root.join(repo_name) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn commits_dir(&self) -> PathBuf {
        self.root.join("commits")
    }

    pub fn commit_dir(&self, record: &CommitRecord) -> anyhow::Result<PathBuf> {
        Ok(self.commits_dir().join(format!("{}-{}", record.day()?, record.short_sha())))
    }

    pub fn digest_dir(&self, day: &str, first_sha: &str, last_sha: &str) -> PathBuf {
        self.root.join("digests").join(format!(
            "{day}-{}..{}",
            short_sha(first_sha),
            short_sha(last_sha)
        ))
    }

    pub fn flame_dir(&self, day: &str) -> PathBuf {
        self.root.join("flame").join(day)
    }

    /// Writes `raw.json`, creating the commit directory; returns the directory
    /// so chart renderers can drop their PNGs next to it.
    pub fn write_commit_record(&self, record: &CommitRecord) -> anyhow::Result<PathBuf> {
        let dir = self.commit_dir(record)?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("raw.json"), serde_json::to_string_pretty(record)?)?;
        Ok(dir)
    }

    /// Loads every `commits/*/raw.json`, sorted by record date (oldest first).
    /// A directory without a parseable `raw.json` is skipped, not fatal — e.g.
    /// a run interrupted between `create_dir_all` and the write.
    pub fn load_commit_records(&self) -> anyhow::Result<Vec<(PathBuf, CommitRecord)>> {
        let commits = self.commits_dir();
        let mut records = Vec::new();
        if !commits.is_dir() {
            return Ok(records);
        }
        for entry in std::fs::read_dir(&commits)? {
            let dir = entry?.path();
            let raw = dir.join("raw.json");
            if !raw.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&raw) else { continue };
            let Ok(record) = serde_json::from_str::<CommitRecord>(&text) else { continue };
            records.push((dir, record));
        }
        records.sort_by(|(_, a), (_, b)| {
            let ka = OffsetDateTime::parse(&a.date, &Rfc3339).ok();
            let kb = OffsetDateTime::parse(&b.date, &Rfc3339).ok();
            ka.cmp(&kb)
        });
        Ok(records)
    }

    /// The most recent `n` commit records, oldest first.
    pub fn load_recent_commit_records(
        &self,
        n: usize,
    ) -> anyhow::Result<Vec<(PathBuf, CommitRecord)>> {
        let mut all = self.load_commit_records()?;
        let skip = all.len().saturating_sub(n);
        Ok(all.split_off(skip))
    }

    /// Exclusive per-repo advisory lock: two invocations for the same repo
    /// share the checkout, the criterion tree, and `state.json`, so they must
    /// never run concurrently. The OS releases the lock when the returned
    /// handle drops — including on crash, so no stale-lock cleanup is needed.
    pub fn acquire_lock(&self) -> anyhow::Result<std::fs::File> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(".lock");
        let file = std::fs::File::create(&path)?;
        file.try_lock().map_err(|e| {
            anyhow::anyhow!(
                "another invocation for this repo is already running (lock {} is held: {e}); \
                 refusing to run concurrently",
                path.display()
            )
        })?;
        Ok(file)
    }

    /// Removes `flame/<day>` directories older than `keep_days` days before
    /// `today` (a `YYYYMMDD` string, lexicographic comparison — same format as
    /// the directory names). Returns the pruned directory names.
    pub fn prune_flame_dirs(&self, today: &str, keep_days: u32) -> anyhow::Result<Vec<String>> {
        let flame_root = self.root.join("flame");
        let mut pruned = Vec::new();
        if !flame_root.is_dir() {
            return Ok(pruned);
        }
        let cutoff = cutoff_day(today, keep_days)?;
        for entry in std::fs::read_dir(&flame_root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && name.as_str() < cutoff.as_str() {
                std::fs::remove_dir_all(entry.path())?;
                pruned.push(name);
            }
        }
        pruned.sort();
        Ok(pruned)
    }
}

/// `today - keep_days` as `YYYYMMDD`.
fn cutoff_day(today: &str, keep_days: u32) -> anyhow::Result<String> {
    let fmt = time::macros::format_description!("[year][month][day]");
    let date = time::Date::parse(today, &fmt)
        .map_err(|e| anyhow::anyhow!("bad YYYYMMDD day '{today}': {e}"))?;
    let cutoff = date - time::Duration::days(i64::from(keep_days));
    Ok(format!("{:04}{:02}{:02}", cutoff.year(), cutoff.month() as u8, cutoff.day()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criterion_results::{RatioRow, WorkloadRatios};

    fn sample_ratios() -> Vec<WorkloadRatios> {
        vec![
            WorkloadRatios {
                group: "salt_dynamic_gas".into(),
                workload: "sstore_100".into(),
                rows: vec![
                    RatioRow {
                        subject: "revm_pinned".into(),
                        mean_ns: 14000.0,
                        ratio_vs_revm_pinned: Some(1.0),
                    },
                    RatioRow {
                        subject: "rex5_salt".into(),
                        mean_ns: 28000.0,
                        ratio_vs_revm_pinned: Some(2.0),
                    },
                ],
            },
            WorkloadRatios {
                group: "empty_transaction".into(),
                workload: String::new(),
                rows: vec![RatioRow {
                    subject: "rex5".into(),
                    mean_ns: 9000.0,
                    ratio_vs_revm_pinned: None,
                }],
            },
        ]
    }

    fn record(sha: &str, date: &str) -> CommitRecord {
        let mut r = CommitRecord::new(sha.into(), date.into(), "rustc 1.86.0".into());
        r.add_ratios(&sample_ratios());
        r
    }

    #[test]
    fn test_record_shape_matches_plan_schema() {
        let r = record("abcdef0123456789", "2026-07-02T10:00:00Z");
        assert_eq!(r.short_sha(), "abcdef0");
        assert_eq!(r.day().unwrap(), "20260702");
        assert_eq!(
            r.groups["salt_dynamic_gas"]["rex5_salt/sstore_100"].ratio_vs_revm_pinned,
            Some(2.0)
        );
        // Bare row (no workload) keys on the subject alone.
        assert_eq!(r.groups["empty_transaction"]["rex5"].ns, 9000.0);
    }

    #[test]
    fn test_write_then_load_roundtrip_and_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        let r = record("abcdef0123456789", "2026-07-02T10:00:00Z");
        let dir = store.write_commit_record(&r).unwrap();
        assert_eq!(dir, tmp.path().join("mega-evm/commits/20260702-abcdef0"));
        assert!(dir.join("raw.json").is_file());

        let loaded = store.load_commit_records().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, r);
    }

    #[test]
    fn test_recent_records_sorted_by_date_not_dirname() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        // Same day, shas that sort backwards lexicographically vs arrival order.
        store.write_commit_record(&record("fff0000000", "2026-07-02T08:00:00Z")).unwrap();
        store.write_commit_record(&record("aaa0000000", "2026-07-02T12:00:00Z")).unwrap();
        store.write_commit_record(&record("bbb0000000", "2026-07-01T23:00:00Z")).unwrap();

        let recent = store.load_recent_commit_records(2).unwrap();
        let shas: Vec<&str> = recent.iter().map(|(_, r)| r.short_sha()).collect();
        assert_eq!(shas, vec!["fff0000", "aaa0000"]);
    }

    #[test]
    fn test_digest_and_flame_dir_naming() {
        let store = RepoStore::new(Path::new("/data"), "mega-evm");
        assert_eq!(
            store.digest_dir("20260702", "abcdef0123", "1234567abc"),
            Path::new("/data/mega-evm/digests/20260702-abcdef0..1234567")
        );
        assert_eq!(store.flame_dir("20260702"), Path::new("/data/mega-evm/flame/20260702"));
    }

    #[test]
    fn test_prune_flame_dirs_keeps_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        for day in ["20260601", "20260620", "20260701"] {
            std::fs::create_dir_all(store.flame_dir(day)).unwrap();
        }
        let pruned = store.prune_flame_dirs("20260702", 14).unwrap();
        assert_eq!(pruned, vec!["20260601".to_string()]);
        assert!(!store.flame_dir("20260601").exists());
        assert!(store.flame_dir("20260620").exists());
        assert!(store.flame_dir("20260701").exists());
    }

    #[test]
    fn test_unparseable_raw_json_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RepoStore::new(tmp.path(), "mega-evm");
        store.write_commit_record(&record("abc0000000", "2026-07-02T08:00:00Z")).unwrap();
        let broken = store.commits_dir().join("20260702-broken0");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("raw.json"), "{not json").unwrap();

        let loaded = store.load_commit_records().unwrap();
        assert_eq!(loaded.len(), 1);
    }
}
