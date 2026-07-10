//! `state.json`: rolling-median baseline per row, the "currently regressed"
//! latch (so an alert fires once on the state *change*, not every run while a
//! regression is active), and the digest-batch counter.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RowHistory {
    /// Recent `ratio_vs_baseline` values, oldest first, capped at the
    /// configured rolling window.
    pub recent_ratios: VecDeque<f64>,
    /// Latched regression state — an alert only fires when this flips, not on
    /// every run while the regression is still active.
    pub currently_regressed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct State {
    pub last_seen_sha: Option<String>,
    pub commits_since_digest: u32,
    pub rows: BTreeMap<String, RowHistory>,
    /// The instructions lane's rolling windows and latches, keyed by the same
    /// row-key strings as `rows`. Empty when the lane is off — and then not
    /// serialized, so pre-lane state files round-trip unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instr_rows: BTreeMap<String, RowHistory>,
}

/// Detection thresholds for [`State::check_and_record`], both in percent over
/// the rolling median.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// A healthy row regresses when it rises more than this.
    pub regression_pct: f64,
    /// A latched row recovers only when back within this (`<=
    /// regression_pct`; equal = no hysteresis). A gap between the two stops a
    /// row oscillating around the regression threshold from emitting an
    /// alternating regression/recovery event pair per run: between the
    /// thresholds it just stays latched (quiet).
    pub recovery_pct: f64,
}

impl Thresholds {
    /// No hysteresis: recover in the same band the row regressed past.
    pub fn uniform(pct: f64) -> Self {
        Self { regression_pct: pct, recovery_pct: pct }
    }
}

/// What `check_and_record` found for one row, relative to its history *before*
/// this run's value is folded in.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// No prior history for this row_key — establishes the baseline, never alerts.
    FirstRun,
    /// Within threshold; nothing to report.
    Ok,
    /// Just crossed the threshold — was not regressed last run, is now. Alert.
    NewRegression { median: f64, current: f64, pct_over: f64 },
    /// Was already regressed and still is — no new alert (avoid spamming).
    StillRegressed,
    /// Was regressed, is back under threshold — send a recovery note.
    Recovered { median: f64, current: f64 },
}

impl Verdict {
    pub fn is_regressed(&self) -> bool {
        matches!(self, Verdict::NewRegression { .. } | Verdict::StillRegressed)
    }
}

fn median(values: &VecDeque<f64>) -> f64 {
    let mut sorted: Vec<f64> = values.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("ratios are always finite"));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

impl State {
    /// A corrupt state file (e.g. a partial write from a crashed run) is
    /// backed up to `<path>.corrupt` and treated as empty rather than
    /// wedging every subsequent run.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        match serde_json::from_str(&text) {
            Ok(state) => Ok(state),
            Err(e) => {
                let backup = path.with_extension("json.corrupt");
                eprintln!(
                    "state file {} is corrupt ({e}); backing it up to {} and starting from an \
                     empty baseline",
                    path.display(),
                    backup.display()
                );
                std::fs::rename(path, &backup)?;
                Ok(Self::default())
            }
        }
    }

    /// Write-to-temp + rename so a crash mid-write can't leave a truncated
    /// state file behind.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Compares `ratio` (this run's `ratio_vs_baseline` for `row_key`)
    /// against the rolling median of its history so far, records the verdict's
    /// regression latch, then folds `ratio` into the window. Call once per
    /// `(row_key, run)` — order matters, this both reads and mutates.
    ///
    /// Regressed values are NOT folded into the window: otherwise a sustained
    /// regression would converge the median onto the regressed level, emit a
    /// false recovery event, and silently rebaseline. The baseline stays
    /// frozen while a row is regressed — a permanent regression stays
    /// `StillRegressed` (quiet after the one alert) until it is actually
    /// fixed, or until the operator accepts the new level via the
    /// `rebaseline` subcommand.
    pub fn check_and_record(
        &mut self,
        row_key: &str,
        ratio: f64,
        thresholds: Thresholds,
        window: usize,
    ) -> Verdict {
        Self::check_and_record_in(&mut self.rows, row_key, ratio, thresholds, window)
    }

    /// [`Self::check_and_record`] for the instructions lane: identical
    /// semantics against the row's history in `instr_rows` — the two lanes'
    /// windows and latches never mix.
    pub fn check_and_record_instr(
        &mut self,
        row_key: &str,
        ratio: f64,
        thresholds: Thresholds,
        window: usize,
    ) -> Verdict {
        Self::check_and_record_in(&mut self.instr_rows, row_key, ratio, thresholds, window)
    }

    fn check_and_record_in(
        rows: &mut BTreeMap<String, RowHistory>,
        row_key: &str,
        ratio: f64,
        thresholds: Thresholds,
        window: usize,
    ) -> Verdict {
        // compute_ratios filters non-finite ratios at the source; a NaN here
        // would otherwise surface as a confusing panic inside median's sort.
        debug_assert!(ratio.is_finite(), "non-finite ratio for {row_key}");
        let entry = rows.entry(row_key.to_string()).or_default();
        let verdict = if entry.recent_ratios.is_empty() {
            Verdict::FirstRun
        } else {
            let baseline_median = median(&entry.recent_ratios);
            let pct_over = (ratio - baseline_median) / baseline_median * 100.0;
            if entry.currently_regressed {
                if pct_over <= thresholds.recovery_pct {
                    Verdict::Recovered { median: baseline_median, current: ratio }
                } else {
                    // Above the recovery threshold — including the hysteresis
                    // gap between the two thresholds: still latched, quiet.
                    Verdict::StillRegressed
                }
            } else if pct_over > thresholds.regression_pct {
                Verdict::NewRegression { median: baseline_median, current: ratio, pct_over }
            } else {
                Verdict::Ok
            }
        };
        entry.currently_regressed = verdict.is_regressed();
        if !verdict.is_regressed() {
            entry.recent_ratios.push_back(ratio);
            while entry.recent_ratios.len() > window {
                entry.recent_ratios.pop_front();
            }
        }
        verdict
    }

    /// Accepts a new performance level for the rows matching `patterns`
    /// (exact row keys or trailing-`*` prefixes): drops their rolling history
    /// AND their regression latch — in BOTH lanes' maps — so the next run
    /// re-baselines them as FirstRun: no alert, fresh window. Returns the
    /// cleared row keys (deduplicated across lanes). This is the `rebaseline`
    /// subcommand's core; the alternative was hand-editing state.json.
    pub fn clear_rows(&mut self, patterns: &[String]) -> Vec<String> {
        let mut matched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for rows in [&mut self.rows, &mut self.instr_rows] {
            let keys: Vec<String> = rows
                .keys()
                .filter(|key| patterns.iter().any(|p| crate::config::star_pattern_matches(p, key)))
                .cloned()
                .collect();
            for key in keys {
                rows.remove(&key);
                matched.insert(key);
            }
        }
        matched.into_iter().collect()
    }

    /// Bumps the digest counter; returns `true` when it has reached
    /// `batch_size` (caller should build+send a digest, then call
    /// `reset_digest_counter`).
    pub fn bump_digest_counter(&mut self, batch_size: u32) -> bool {
        self.commits_since_digest += 1;
        self.commits_since_digest >= batch_size
    }

    pub fn reset_digest_counter(&mut self) {
        self.commits_since_digest = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_run_establishes_baseline_no_alert() {
        let mut state = State::default();
        let verdict = state.check_and_record(
            "salt_dynamic_gas/rex5_salt/sstore_100",
            2.0,
            Thresholds::uniform(10.0),
            20,
        );
        assert_eq!(verdict, Verdict::FirstRun);
        assert!(!verdict.is_regressed());
        assert_eq!(state.rows["salt_dynamic_gas/rex5_salt/sstore_100"].recent_ratios.len(), 1);
    }

    #[test]
    fn test_regression_over_threshold_fires_once_then_still_regressed() {
        let mut state = State::default();
        let key = "g/rex5/w";
        // Build up a stable history around ratio 2.0.
        for _ in 0..5 {
            state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
        }
        // 15% jump — over the 10% threshold.
        let v1 = state.check_and_record(key, 2.3, Thresholds::uniform(10.0), 20);
        match v1 {
            Verdict::NewRegression { median, current, pct_over } => {
                assert!((median - 2.0).abs() < 1e-9);
                assert!((current - 2.3).abs() < 1e-9);
                assert!(pct_over > 10.0);
            }
            other => panic!("expected NewRegression, got {other:?}"),
        }
        // Still elevated next run — must NOT re-alert (StillRegressed, not NewRegression).
        let v2 = state.check_and_record(key, 2.35, Thresholds::uniform(10.0), 20);
        assert_eq!(v2, Verdict::StillRegressed);
        assert!(v2.is_regressed());
    }

    #[test]
    fn test_improvement_does_not_regress() {
        let mut state = State::default();
        let key = "g/rex5/w";
        for _ in 0..5 {
            state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
        }
        let v = state.check_and_record(key, 1.5, Thresholds::uniform(10.0), 20);
        assert_eq!(v, Verdict::Ok);
    }

    #[test]
    fn test_missing_baseline_is_first_run_not_a_crash() {
        // A row_key never seen before (e.g. a brand new workload) is FirstRun,
        // not treated as a 0% -> N% "regression".
        let mut state = State::default();
        let v =
            state.check_and_record("new/workload/never/seen", 999.0, Thresholds::uniform(10.0), 20);
        assert_eq!(v, Verdict::FirstRun);
    }

    #[test]
    fn test_recovery_fires_once_after_a_regression() {
        let mut state = State::default();
        let key = "g/rex5/w";
        for _ in 0..5 {
            state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
        }
        let regressed = state.check_and_record(key, 2.3, Thresholds::uniform(10.0), 20);
        assert!(regressed.is_regressed());
        // Drops back near baseline.
        let v = state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
        match v {
            Verdict::Recovered { .. } => {}
            other => panic!("expected Recovered, got {other:?}"),
        }
        assert!(!v.is_regressed());
        // Next run at the same level: no repeated recovery notice, just Ok.
        let v2 = state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
        assert_eq!(v2, Verdict::Ok);
    }

    #[test]
    fn test_sustained_regression_never_falsely_recovers() {
        // The regressed values must not be folded into the window: a +50%
        // regression held for many runs stays StillRegressed (frozen baseline)
        // instead of converging the median and emitting a false Recovered.
        let mut state = State::default();
        let key = "g/rex5/w";
        for _ in 0..5 {
            state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
        }
        assert!(matches!(
            state.check_and_record(key, 3.0, Thresholds::uniform(10.0), 20),
            Verdict::NewRegression { .. }
        ));
        for run in 0..30 {
            let v = state.check_and_record(key, 3.0, Thresholds::uniform(10.0), 20);
            assert_eq!(v, Verdict::StillRegressed, "run {run} must stay regressed");
        }
        // The baseline is still the pre-regression 2.0 median, so an actual
        // fix back to 2.05 recovers against the ORIGINAL baseline.
        match state.check_and_record(key, 2.05, Thresholds::uniform(10.0), 20) {
            Verdict::Recovered { median, .. } => assert!((median - 2.0).abs() < 1e-9),
            other => panic!("expected Recovered, got {other:?}"),
        }
        // And the window was not polluted by the regressed era: a fresh +15%
        // over the old level alerts again.
        assert!(matches!(
            state.check_and_record(key, 2.35, Thresholds::uniform(10.0), 20),
            Verdict::NewRegression { .. }
        ));
    }

    #[test]
    fn test_recovery_hysteresis_holds_the_latch_between_thresholds() {
        // Regress past +10%, recover only under +5%: the gap in between must
        // stay latched and quiet instead of emitting recovery/regression
        // pairs while the row oscillates around the regression threshold.
        let th = Thresholds { regression_pct: 10.0, recovery_pct: 5.0 };
        let mut state = State::default();
        let key = "g/rex5/w";
        for _ in 0..5 {
            state.check_and_record(key, 2.0, th, 20);
        }
        assert!(matches!(state.check_and_record(key, 2.3, th, 20), Verdict::NewRegression { .. }));
        // +7% over the frozen median: within the old one-threshold band, but
        // above the recovery threshold — no recovery yet.
        assert_eq!(state.check_and_record(key, 2.14, th, 20), Verdict::StillRegressed);
        // Oscillating back up must NOT re-alert (still the same latch).
        assert_eq!(state.check_and_record(key, 2.3, th, 20), Verdict::StillRegressed);
        // +4%: under the recovery threshold — one recovery event, and the
        // hysteresis-gap values never polluted the frozen window.
        match state.check_and_record(key, 2.08, th, 20) {
            Verdict::Recovered { median, .. } => assert!((median - 2.0).abs() < 1e-9),
            other => panic!("expected Recovered, got {other:?}"),
        }
        // Healthy again: a fresh +15% re-alerts against the old baseline.
        assert!(matches!(state.check_and_record(key, 2.31, th, 20), Verdict::NewRegression { .. }));
    }

    #[test]
    fn test_clear_rows_drops_history_and_latch_by_pattern() {
        let mut state = State::default();
        // Two latched regressed rows and one healthy row.
        for key in ["g/rex5_salt/w", "g/rex5/w", "other/rex5/w"] {
            for _ in 0..3 {
                state.check_and_record(key, 2.0, Thresholds::uniform(10.0), 20);
            }
        }
        assert!(state
            .check_and_record("g/rex5_salt/w", 3.0, Thresholds::uniform(10.0), 20)
            .is_regressed());
        assert!(state
            .check_and_record("g/rex5/w", 3.0, Thresholds::uniform(10.0), 20)
            .is_regressed());

        let cleared = state.clear_rows(&["g/*".to_string()]);
        assert_eq!(cleared, vec!["g/rex5/w".to_string(), "g/rex5_salt/w".to_string()]);
        // The untouched row keeps its window.
        assert_eq!(state.rows.len(), 1);
        assert!(state.rows.contains_key("other/rex5/w"));

        // Cleared rows re-baseline at the NEW level: FirstRun, no alert, and
        // a later value near the new level stays Ok.
        assert_eq!(
            state.check_and_record("g/rex5_salt/w", 3.0, Thresholds::uniform(10.0), 20),
            Verdict::FirstRun
        );
        assert_eq!(
            state.check_and_record("g/rex5_salt/w", 3.05, Thresholds::uniform(10.0), 20),
            Verdict::Ok
        );

        // A pattern matching nothing clears nothing.
        assert!(state.clear_rows(&["nope/*".to_string()]).is_empty());
    }

    #[test]
    fn test_corrupt_state_file_is_backed_up_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        std::fs::write(&path, "{truncated").unwrap();
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded, State::default());
        assert!(path.with_extension("json.corrupt").is_file());
        assert!(!path.exists());
    }

    #[test]
    fn test_rolling_window_caps_at_20() {
        let mut state = State::default();
        let key = "g/s/w";
        for i in 0..30 {
            state.check_and_record(key, 1.0 + i as f64 * 0.01, Thresholds::uniform(10.0), 20);
        }
        assert_eq!(state.rows[key].recent_ratios.len(), 20);
    }

    #[test]
    fn test_digest_counter_batches_at_ten_then_resets() {
        let mut state = State::default();
        for i in 0..9 {
            assert!(!state.bump_digest_counter(10), "should not fire before 10 (i={i})");
        }
        assert!(state.bump_digest_counter(10), "10th commit should fire the digest");
        state.reset_digest_counter();
        assert_eq!(state.commits_since_digest, 0);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let mut state = State::default();
        state.check_and_record("g/s/w", 1.5, Thresholds::uniform(10.0), 20);
        state.last_seen_sha = Some("abc123".into());
        state.commits_since_digest = 3;
        state.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn test_load_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded, State::default());
    }
}
