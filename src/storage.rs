//! Categorized local storage under `<data-root>/<repo>/`:
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
use crate::instructions::{InstrCollection, InstrRow, InstrWorkloadRatios};
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
    /// `mean_ns / baseline mean_ns` for the same `(group, workload)`;
    /// `None` when the group/workload has no baseline row. The baseline
    /// subject's name is recorded at the record level (`baseline_subject`).
    pub ratio_vs_baseline: Option<f64>,
    /// Instruction-count numbers; absent (not `null`) when the instructions
    /// lane was off or produced nothing for this row, so lane-off records are
    /// byte-identical to pre-lane ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instr: Option<InstrRecord>,
}

/// One row's instruction-count numbers inside `raw.json` (`instr` field).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstrRecord {
    /// CPU instructions retired (callgrind `Ir`) for one traced iteration.
    pub count: u64,
    /// `count / baseline count` for the same `(group, workload)`; `None`
    /// when the group/workload has no baseline count.
    pub ratio_vs_baseline: Option<f64>,
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
    /// The subject every `ratio_vs_baseline` in this record is against.
    pub baseline_subject: String,
    /// Bench targets that failed to compile or run this commit. Marked, not
    /// silently dropped; their rows are simply absent from `groups`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_targets: Vec<String>,
    /// Bench targets whose instructions-lane build/run failed. Absent when
    /// the lane is off or every target collected fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instr_failed_targets: Option<Vec<String>>,
    /// `group -> row ("subject" or "subject/workload") -> numbers`.
    pub groups: BTreeMap<String, BTreeMap<String, RowRecord>>,
}

impl CommitRecord {
    pub fn new(commit: String, date: String, rustc: String, baseline_subject: String) -> Self {
        Self {
            commit,
            date,
            rustc,
            baseline_subject,
            failed_targets: Vec::new(),
            instr_failed_targets: None,
            groups: BTreeMap::new(),
        }
    }

    /// Folds parsed ratio tables into `groups`.
    pub fn add_ratios(&mut self, ratios: &[WorkloadRatios]) {
        for wl in ratios {
            let group = self.groups.entry(wl.group.clone()).or_default();
            for row in &wl.rows {
                group.insert(
                    row_name(&row.subject, &wl.workload),
                    RowRecord {
                        ns: row.mean_ns,
                        ratio_vs_baseline: row.ratio_vs_baseline,
                        instr: None,
                    },
                );
            }
        }
    }

    /// Attaches the instructions lane's counts/ratios to existing rows. A
    /// count whose row the walltime lane did not produce (e.g. its walltime
    /// target failed while the instrumented run succeeded) has no `RowRecord`
    /// to attach to — `ns` is a required field of the stable schema — and is
    /// skipped with a stderr note; its history is still tracked in
    /// `state.json`.
    pub fn add_instr_ratios(&mut self, ratios: &[InstrWorkloadRatios]) {
        for wl in ratios {
            for row in &wl.rows {
                let name = row_name(&row.subject, &wl.workload);
                match self.groups.get_mut(&wl.group).and_then(|g| g.get_mut(&name)) {
                    Some(record) => {
                        record.instr = Some(InstrRecord {
                            count: row.count,
                            ratio_vs_baseline: row.ratio_vs_baseline,
                        });
                    }
                    None => eprintln!(
                        "instructions lane: no walltime row for {}/{name} — count not recorded \
                         in raw.json",
                        wl.group
                    ),
                }
            }
        }
    }

    /// Reconstructs the instructions collection embedded in this record's
    /// rows — the carry-forward source for `--skip-bench` regeneration, which
    /// never re-collects the lane. The row-name split is lossless because a
    /// subject never contains `/` (criterion's function_id splits at the
    /// FIRST `/`, exactly like this). `None` when the record carries no
    /// instructions data at all, so a walltime-only record regenerates
    /// byte-identically to today.
    pub fn instr_collection(&self) -> Option<InstrCollection> {
        let mut rows = Vec::new();
        for (group, group_rows) in &self.groups {
            for (name, row) in group_rows {
                let Some(instr) = &row.instr else { continue };
                let (subject, workload) = match name.split_once('/') {
                    Some((subject, workload)) => (subject.to_string(), workload.to_string()),
                    None => (name.clone(), String::new()),
                };
                rows.push(InstrRow { group: group.clone(), subject, workload, count: instr.count });
            }
        }
        let failed_targets = self.instr_failed_targets.clone().unwrap_or_default();
        if rows.is_empty() && failed_targets.is_empty() {
            return None;
        }
        Some(InstrCollection { rows, failed_targets })
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

    /// Ad-hoc (manual `trend` subcommand) window dir — same naming scheme as
    /// digests, under `trends/` so the automatic digests stay untouched.
    pub fn trend_dir(&self, day: &str, first_sha: &str, last_sha: &str) -> PathBuf {
        self.root.join("trends").join(format!(
            "{day}-{}..{}",
            short_sha(first_sha),
            short_sha(last_sha)
        ))
    }

    pub fn flame_dir(&self, day: &str) -> PathBuf {
        self.root.join("flame").join(day)
    }

    /// Writes `raw.json`, creating the commit directory; returns the directory
    /// so chart renderers can drop their PNGs next to it. Atomic: the `trend`
    /// subcommand reads records without taking the repo lock, and a torn
    /// raw.json would be silently skipped from its window.
    pub fn write_commit_record(&self, record: &CommitRecord) -> anyhow::Result<PathBuf> {
        let dir = self.commit_dir(record)?;
        std::fs::create_dir_all(&dir)?;
        write_atomic(&dir.join("raw.json"), &serde_json::to_string_pretty(record)?)?;
        Ok(dir)
    }

    /// Loads the stored `raw.json` for one commit's directory (located by
    /// `record`'s date + sha, the same way [`Self::commit_dir`] names it).
    /// `Ok(None)` = nothing stored there yet; `Err` = a raw.json is present
    /// but unreadable or unparseable — the `--skip-bench` carry-forward
    /// caller warns and regenerates without it instead of failing.
    pub fn load_commit_record(
        &self,
        record: &CommitRecord,
    ) -> anyhow::Result<Option<CommitRecord>> {
        let path = self.commit_dir(record)?.join("raw.json");
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let parsed = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok(Some(parsed))
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

    /// `latest.json` — an atomic pointer to the most recently completed run:
    /// `{sha, commit_dir, finished_at}`. Consumers compare `sha` against their
    /// own last-processed marker to decide whether there is anything new.
    pub fn write_latest(&self, sha: &str, commit_dir: &Path) -> anyhow::Result<()> {
        let latest = serde_json::json!({
            "sha": sha,
            "commit_dir": commit_dir,
            "finished_at": time::OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
        });
        write_atomic(&self.root.join("latest.json"), &serde_json::to_string_pretty(&latest)?)
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

/// Write-to-temp + rename so concurrent readers never observe a torn file
/// and a crash mid-write can't leave a truncated one behind.
fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
                        ratio_vs_baseline: Some(1.0),
                    },
                    RatioRow {
                        subject: "rex5_salt".into(),
                        mean_ns: 28000.0,
                        ratio_vs_baseline: Some(2.0),
                    },
                ],
            },
            WorkloadRatios {
                group: "empty_transaction".into(),
                workload: String::new(),
                rows: vec![RatioRow {
                    subject: "rex5".into(),
                    mean_ns: 9000.0,
                    ratio_vs_baseline: None,
                }],
            },
        ]
    }

    fn record(sha: &str, date: &str) -> CommitRecord {
        let mut r =
            CommitRecord::new(sha.into(), date.into(), "rustc 1.86.0".into(), "revm_pinned".into());
        r.add_ratios(&sample_ratios());
        r
    }

    /// `raw.json` bytes for a lane-off record, captured by serializing this
    /// exact record with the pre-instructions-lane code. Guards the core
    /// compatibility contract: with the lane off, the record must serialize
    /// byte-identically to before the lane existed.
    const PRE_LANE_GOLDEN: &str = r#"{
  "commit": "abcdef0123456789abcdef0123456789abcdef01",
  "date": "2026-07-02T10:00:00Z",
  "rustc": "rustc 1.86.0 (05f9846f8 2025-03-31)",
  "baseline_subject": "revm_pinned",
  "failed_targets": [
    "block_bench"
  ],
  "groups": {
    "empty_transaction": {
      "rex5": {
        "ns": 9000.0,
        "ratio_vs_baseline": null
      }
    },
    "salt_dynamic_gas": {
      "revm_pinned/sstore_100": {
        "ns": 14000.0,
        "ratio_vs_baseline": 1.0
      },
      "rex5_salt/sstore_100": {
        "ns": 28000.0,
        "ratio_vs_baseline": 2.0
      }
    }
  }
}"#;

    #[test]
    fn test_lane_off_record_serializes_byte_identical_to_pre_lane_golden() {
        let mut record = CommitRecord::new(
            "abcdef0123456789abcdef0123456789abcdef01".into(),
            "2026-07-02T10:00:00Z".into(),
            "rustc 1.86.0 (05f9846f8 2025-03-31)".into(),
            "revm_pinned".into(),
        );
        record.add_ratios(&sample_ratios());
        record.failed_targets = vec!["block_bench".to_string()];
        assert_eq!(serde_json::to_string_pretty(&record).unwrap(), PRE_LANE_GOLDEN);
        // And a pre-lane file (no instr fields) still deserializes.
        let parsed: CommitRecord = serde_json::from_str(PRE_LANE_GOLDEN).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn test_instr_ratios_attach_to_rows_and_roundtrip() {
        let mut record = record("abcdef0123456789", "2026-07-02T10:00:00Z");
        record.add_instr_ratios(&[
            InstrWorkloadRatios {
                group: "salt_dynamic_gas".into(),
                workload: "sstore_100".into(),
                rows: vec![crate::instructions::InstrRatioRow {
                    subject: "rex5_salt".into(),
                    count: 25_000,
                    ratio_vs_baseline: Some(2.5),
                }],
            },
            // A count without a walltime row is skipped, not a panic and not
            // a phantom RowRecord.
            InstrWorkloadRatios {
                group: "missing_group".into(),
                workload: String::new(),
                rows: vec![crate::instructions::InstrRatioRow {
                    subject: "rex5".into(),
                    count: 1,
                    ratio_vs_baseline: None,
                }],
            },
        ]);
        let row = &record.groups["salt_dynamic_gas"]["rex5_salt/sstore_100"];
        assert_eq!(row.instr, Some(InstrRecord { count: 25_000, ratio_vs_baseline: Some(2.5) }));
        // The sibling row without instr data keeps its plain shape.
        assert_eq!(record.groups["salt_dynamic_gas"]["revm_pinned/sstore_100"].instr, None);
        assert!(!record.groups.contains_key("missing_group"));

        let json = serde_json::to_string_pretty(&record).unwrap();
        let parsed: CommitRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn test_instr_collection_reconstructs_rows_and_failed_targets() {
        // The carry-forward source: counts come back with the exact
        // (group, subject, workload) triples they were stored under —
        // including a bare row (no workload) — plus the failure markers.
        let mut rec = record("abcdef0123456789", "2026-07-02T10:00:00Z");
        rec.add_instr_ratios(&[
            InstrWorkloadRatios {
                group: "salt_dynamic_gas".into(),
                workload: "sstore_100".into(),
                rows: vec![crate::instructions::InstrRatioRow {
                    subject: "rex5_salt".into(),
                    count: 25_000,
                    ratio_vs_baseline: Some(2.5),
                }],
            },
            InstrWorkloadRatios {
                group: "empty_transaction".into(),
                workload: String::new(),
                rows: vec![crate::instructions::InstrRatioRow {
                    subject: "rex5".into(),
                    count: 9_000,
                    ratio_vs_baseline: None,
                }],
            },
        ]);
        rec.instr_failed_targets = Some(vec!["block_bench".to_string()]);

        let collection = rec.instr_collection().expect("instr data present");
        assert_eq!(collection.failed_targets, vec!["block_bench".to_string()]);
        let mut rows = collection.rows;
        rows.sort_by(|a, b| (&a.group, &a.subject).cmp(&(&b.group, &b.subject)));
        assert_eq!(
            rows,
            vec![
                InstrRow {
                    group: "empty_transaction".into(),
                    subject: "rex5".into(),
                    workload: String::new(),
                    count: 9_000,
                },
                InstrRow {
                    group: "salt_dynamic_gas".into(),
                    subject: "rex5_salt".into(),
                    workload: "sstore_100".into(),
                    count: 25_000,
                },
            ]
        );

        // A record without any instructions data has nothing to carry.
        assert_eq!(record("abcdef0123456789", "2026-07-02T10:00:00Z").instr_collection(), None);
    }

    #[test]
    fn test_record_shape_matches_plan_schema() {
        let r = record("abcdef0123456789", "2026-07-02T10:00:00Z");
        assert_eq!(r.short_sha(), "abcdef0");
        assert_eq!(r.day().unwrap(), "20260702");
        assert_eq!(
            r.groups["salt_dynamic_gas"]["rex5_salt/sstore_100"].ratio_vs_baseline,
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
        // Atomic write: no temp file may survive next to the record.
        assert!(!dir.join("raw.json.tmp").exists());

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
