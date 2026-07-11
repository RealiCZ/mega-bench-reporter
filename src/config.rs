//! Repo-config list: which repos are tracked, what to bench, and how their
//! subjects are interpreted. One entry today (mega-evm); the list shape is
//! what leaves room for more.

use crate::lane::Lane;
use crate::state::Thresholds;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RepoConfig {
    pub name: String,
    pub github: String,
    pub branch: String,
    pub clone_url: String,
    pub bench_targets: Vec<String>,
    /// The subject every ratio is computed against (e.g. `revm_pinned`).
    pub baseline_subject: String,
    /// Subject patterns that headline this repo: exact names or trailing-`*`
    /// prefixes (e.g. `["rex5", "rex5_*"]`). Headline rows drive regression
    /// events, the ratio column, and digests.
    pub headline_subjects: Vec<String>,
    /// Optional display order for subjects in the comparison table; unlisted
    /// subjects follow alphabetically. Defaults to baseline-first.
    #[serde(default)]
    pub subject_order: Option<Vec<String>>,
    /// Cargo package the bench targets live in; defaults to `name`.
    #[serde(default)]
    pub package: Option<String>,
    /// Per-repo tuning overrides; anything unset falls back to `[defaults]`,
    /// then to the built-in values.
    #[serde(flatten)]
    pub tuning: Tuning,
    /// Nightly flame-graph settings; absent = the `flamegraph` subcommand is
    /// not available for this repo.
    #[serde(default)]
    pub flamegraph: Option<FlamegraphConfig>,
    /// Instruction-count lane settings; absent = the lane is off and the
    /// output is byte-identical to a walltime-only run.
    #[serde(default)]
    pub instructions: Option<InstructionsConfig>,
}

impl RepoConfig {
    pub fn package(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.name)
    }

    /// Does `subject` match any headline pattern (exact, or trailing-`*`
    /// prefix)?
    pub fn is_headline(&self, subject: &str) -> bool {
        self.headline_subjects.iter().any(|pattern| star_pattern_matches(pattern, subject))
    }

    /// Human-readable label for the headline family, e.g. `rex5, rex5_*`.
    pub fn headline_label(&self) -> String {
        self.headline_subjects.join(", ")
    }

    /// Resolved subject display order: configured list, or baseline-first.
    pub fn subject_order(&self) -> Vec<String> {
        self.subject_order.clone().unwrap_or_else(|| vec![self.baseline_subject.clone()])
    }
}

/// `true` when `value` matches `pattern` — exact, or prefix when the pattern
/// ends with `*`. The one pattern grammar of every user-facing filter:
/// `headline_subjects` entries and the `trend --row` selectors.
pub fn star_pattern_matches(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

/// The tunable knobs, all optional at both the `[defaults]` and per-repo
/// level. Resolution order: repo value → `[defaults]` value → built-in.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Tuning {
    /// Alert when a headline row rises more than this % over its rolling
    /// median (built-in: 10).
    pub regression_threshold_pct: Option<f64>,
    /// A latched row recovers only when back within this % of its frozen
    /// median (hysteresis; must be <= the regression threshold). Unset = the
    /// regression threshold, i.e. no hysteresis.
    pub recovery_threshold_pct: Option<f64>,
    /// How many healthy runs feed the rolling median (built-in: 20).
    pub rolling_window: Option<usize>,
    /// Commits per trend digest (built-in: 10).
    pub digest_batch_size: Option<u32>,
    /// Cargo profile for `cargo bench` runs. Unset = cargo's default bench
    /// profile — comparable with the scheduled walltime layer. Set to
    /// `"profiling"` to bench the debug-symbol build the flamegraph pipeline
    /// uses.
    pub bench_profile: Option<String>,
    /// Instructions lane: alert when a headline row's instruction-count ratio
    /// rises more than this % over its rolling median (built-in: 2). Counts
    /// are deterministic, so the threshold only needs headroom for incidental
    /// codegen shifts, not run-to-run noise.
    pub instr_regression_threshold_pct: Option<f64>,
    /// Instructions lane recovery threshold (hysteresis; must be <= the
    /// instructions regression threshold). Unset = that threshold.
    pub instr_recovery_threshold_pct: Option<f64>,
}

/// Built-in defaults used when neither the repo nor `[defaults]` sets a knob.
const DEFAULT_REGRESSION_THRESHOLD_PCT: f64 = 10.0;
const DEFAULT_INSTR_REGRESSION_THRESHOLD_PCT: f64 = 2.0;
const DEFAULT_ROLLING_WINDOW: usize = 20;
const DEFAULT_DIGEST_BATCH_SIZE: u32 = 10;

/// Fully-resolved settings for one repo.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub regression_threshold_pct: f64,
    /// Defaults to `regression_threshold_pct` when not configured.
    pub recovery_threshold_pct: f64,
    /// Instructions lane; unused (but still resolved) when the lane is off.
    pub instr_regression_threshold_pct: f64,
    /// Defaults to `instr_regression_threshold_pct` when not configured.
    pub instr_recovery_threshold_pct: f64,
    pub rolling_window: usize,
    pub digest_batch_size: u32,
    pub bench_profile: Option<String>,
}

impl Settings {
    /// The resolved (regression, recovery) threshold pair for one lane.
    pub fn thresholds(&self, lane: Lane) -> Thresholds {
        match lane {
            Lane::Walltime => Thresholds {
                regression_pct: self.regression_threshold_pct,
                recovery_pct: self.recovery_threshold_pct,
            },
            Lane::Instructions => Thresholds {
                regression_pct: self.instr_regression_threshold_pct,
                recovery_pct: self.instr_recovery_threshold_pct,
            },
        }
    }

    fn resolve(repo: &Tuning, defaults: &Tuning) -> anyhow::Result<Self> {
        // One resolve + one validate per lane: the walltime and instructions
        // pairs share the same (regression, recovery) logic and rules, called
        // twice with each lane's config keys and built-in default.
        let pick = |repo_val: Option<f64>, default_val: Option<f64>| repo_val.or(default_val);
        let walltime = Thresholds::resolve(
            pick(repo.regression_threshold_pct, defaults.regression_threshold_pct),
            pick(repo.recovery_threshold_pct, defaults.recovery_threshold_pct),
            DEFAULT_REGRESSION_THRESHOLD_PCT,
        );
        let instr = Thresholds::resolve(
            pick(repo.instr_regression_threshold_pct, defaults.instr_regression_threshold_pct),
            pick(repo.instr_recovery_threshold_pct, defaults.instr_recovery_threshold_pct),
            DEFAULT_INSTR_REGRESSION_THRESHOLD_PCT,
        );
        // Nonsense values would silently disable alerting or spam it — reject
        // loudly, walltime pair first, then the instructions pair.
        walltime.validate("")?;
        instr.validate("instr_")?;

        let settings = Self {
            regression_threshold_pct: walltime.regression_pct,
            recovery_threshold_pct: walltime.recovery_pct,
            instr_regression_threshold_pct: instr.regression_pct,
            instr_recovery_threshold_pct: instr.recovery_pct,
            rolling_window: repo
                .rolling_window
                .or(defaults.rolling_window)
                .unwrap_or(DEFAULT_ROLLING_WINDOW),
            digest_batch_size: repo
                .digest_batch_size
                .or(defaults.digest_batch_size)
                .unwrap_or(DEFAULT_DIGEST_BATCH_SIZE),
            bench_profile: repo.bench_profile.clone().or_else(|| defaults.bench_profile.clone()),
        };
        // A zero window makes every run FirstRun (alerting off); a zero batch
        // size would divide the digest cadence by nothing.
        if settings.rolling_window == 0 {
            anyhow::bail!("rolling_window must be >= 1");
        }
        if settings.digest_batch_size == 0 {
            anyhow::bail!("digest_batch_size must be >= 1");
        }
        Ok(settings)
    }
}

/// Nightly flame-graph settings. The workload set is config, not code, so
/// adjusting which benchmark ids get profiled is a config change.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FlamegraphConfig {
    /// The bench target (`--bench <this>`) containing the workloads.
    pub bench_target: String,
    /// Seconds passed to criterion's `--profile-time` per workload.
    #[serde(default = "default_profile_secs")]
    pub profile_secs: u64,
    /// Days of `flame/<date>/` directories to keep.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Baseline/feature benchmark-id pairs; each side gets its own SVG and the
    /// pair gets a differential SVG (feature vs baseline).
    pub workloads: Vec<FlameWorkloadPair>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FlameWorkloadPair {
    /// Full benchmark id, e.g. `salt_dynamic_gas/revm_pinned/sstore_100`.
    pub baseline: String,
    /// Full benchmark id, e.g. `salt_dynamic_gas/rex5_salt/sstore_100`.
    pub feature: String,
}

fn default_profile_secs() -> u64 {
    30
}

fn default_retention_days() -> u32 {
    30
}

/// Instruction-count lane settings (CodSpeed runner offline mode). Presence
/// of the section turns the lane on; it only ever runs on Linux hosts with
/// the `codspeed` CLI and `cargo-codspeed` installed.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct InstructionsConfig {
    /// Optional benchmark filter appended to `cargo codspeed run`; unset =
    /// run everything the instrumented build produced.
    #[serde(default)]
    pub bench_filter: Option<String>,
    /// `false` (the default) keeps the lane best-effort: a lane skip or a
    /// lane-failed target never fails the run. `true` makes such a run exit
    /// nonzero — but only after the complete happy-path write sequence
    /// (walltime artifacts, events, state, latest.json), so the on-disk data
    /// stays valid and the nonzero exit is purely a signal to the scheduler.
    #[serde(default)]
    pub require_instructions: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Config {
    /// Global tuning defaults, overridable per repo.
    #[serde(default)]
    pub defaults: Tuning,
    #[serde(rename = "repos")]
    pub repos: Vec<RepoConfig>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        if cfg.repos.is_empty() {
            anyhow::bail!("config has no [[repos]] entries");
        }
        Ok(cfg)
    }

    pub fn repo(&self, name: &str) -> anyhow::Result<&RepoConfig> {
        self.repos
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("no repo named '{name}' in config"))
    }

    /// Resolved settings for a repo: per-repo overrides over `[defaults]`
    /// over built-ins. Errors on out-of-range values.
    pub fn settings(&self, repo: &RepoConfig) -> anyhow::Result<Settings> {
        Settings::resolve(&repo.tuning, &self.defaults)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[repos]]
name = "mega-evm"
github = "megaeth-labs/mega-evm"
branch = "main"
clone_url = "git@github.com:megaeth-labs/mega-evm.git"
bench_targets = ["transact", "revm_bench", "mega_bench", "comp_cost", "block_bench"]
baseline_subject = "revm_pinned"
headline_subjects = ["rex5", "rex5_*"]
"#;

    #[test]
    fn test_star_pattern_matches() {
        assert!(star_pattern_matches("rex5", "rex5"));
        assert!(!star_pattern_matches("rex5", "rex5_salt"));
        assert!(star_pattern_matches("rex5_*", "rex5_salt"));
        assert!(!star_pattern_matches("rex5_*", "rex5"));
        // A lone `*` is the match-everything pattern.
        assert!(star_pattern_matches("*", "anything/at/all"));
        // The star is only a wildcard at the end.
        assert!(!star_pattern_matches("*_salt", "rex5_salt"));
    }

    #[test]
    fn test_parse_single_repo() {
        let cfg = Config::parse(SAMPLE).expect("parses");
        assert_eq!(cfg.repos.len(), 1);
        let repo = cfg.repo("mega-evm").expect("found");
        assert_eq!(repo.github, "megaeth-labs/mega-evm");
        assert_eq!(repo.branch, "main");
        assert_eq!(repo.baseline_subject, "revm_pinned");
        assert_eq!(repo.bench_targets.len(), 5);
        assert!(repo.is_headline("rex5"));
        assert!(repo.is_headline("rex5_salt"));
        assert!(!repo.is_headline("rex4"));
        assert!(!repo.is_headline("revm_pinned"));
        assert_eq!(repo.subject_order(), vec!["revm_pinned".to_string()]);
    }

    #[test]
    fn test_parse_multiple_repos_is_a_list_not_a_special_case() {
        let two = format!(
            "{SAMPLE}\n[[repos]]\nname = \"mega-reth\"\ngithub = \"megaeth-labs/mega-reth\"\n\
             branch = \"main\"\nclone_url = \"git@github.com:megaeth-labs/mega-reth.git\"\n\
             bench_targets = []\nbaseline_subject = \"reth_pinned\"\nheadline_subjects = [\"mega\"]\n"
        );
        let cfg = Config::parse(&two).expect("parses");
        assert_eq!(cfg.repos.len(), 2);
        assert!(cfg.repo("mega-reth").is_ok());
    }

    #[test]
    fn test_unknown_repo_name_errors() {
        let cfg = Config::parse(SAMPLE).expect("parses");
        assert!(cfg.repo("does-not-exist").is_err());
    }

    #[test]
    fn test_empty_repos_list_errors() {
        let err = Config::parse("repos = []").unwrap_err();
        assert!(err.to_string().contains("no [[repos]] entries"));
    }

    #[test]
    fn test_settings_built_in_defaults_when_nothing_configured() {
        let cfg = Config::parse(SAMPLE).expect("parses");
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap()).unwrap();
        assert_eq!(settings.regression_threshold_pct, 10.0);
        // Unset recovery threshold = the regression threshold (no hysteresis).
        assert_eq!(settings.recovery_threshold_pct, 10.0);
        // The instructions lane's deterministic counts get a tight built-in.
        assert_eq!(settings.instr_regression_threshold_pct, 2.0);
        assert_eq!(settings.instr_recovery_threshold_pct, 2.0);
        assert_eq!(settings.rolling_window, 20);
        assert_eq!(settings.digest_batch_size, 10);
        assert_eq!(settings.bench_profile, None);
    }

    #[test]
    fn test_instructions_lane_off_by_default_and_parses_with_or_without_filter() {
        let cfg = Config::parse(SAMPLE).expect("parses");
        assert_eq!(cfg.repo("mega-evm").unwrap().instructions, None);

        // Empty section = lane on, no filter, best-effort (knob defaults off).
        let cfg = Config::parse(&format!("{SAMPLE}\n[repos.instructions]\n")).expect("parses");
        let instr = cfg.repo("mega-evm").unwrap().instructions.as_ref().expect("lane on");
        assert_eq!(instr.bench_filter, None);
        assert!(!instr.require_instructions, "require_instructions defaults to false");

        let cfg = Config::parse(&format!(
            "{SAMPLE}\n[repos.instructions]\nbench_filter = \"mega_bench\"\n"
        ))
        .expect("parses");
        let instr = cfg.repo("mega-evm").unwrap().instructions.as_ref().expect("lane on");
        assert_eq!(instr.bench_filter.as_deref(), Some("mega_bench"));

        let cfg = Config::parse(&format!(
            "{SAMPLE}\n[repos.instructions]\nrequire_instructions = true\n"
        ))
        .expect("parses");
        let instr = cfg.repo("mega-evm").unwrap().instructions.as_ref().expect("lane on");
        assert!(instr.require_instructions);
    }

    #[test]
    fn test_settings_instr_recovery_threshold_follows_instr_regression_threshold() {
        // Only the instructions regression threshold set: recovery follows it.
        let cfg = Config::parse(&format!("{SAMPLE}\ninstr_regression_threshold_pct = 1.0\n"))
            .expect("parses");
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap()).unwrap();
        assert_eq!(settings.instr_regression_threshold_pct, 1.0);
        assert_eq!(settings.instr_recovery_threshold_pct, 1.0);
        // The instructions pair is independent of the walltime pair.
        assert_eq!(settings.regression_threshold_pct, 10.0);
    }

    #[test]
    fn test_settings_recovery_threshold_follows_configured_regression_threshold() {
        // Only the regression threshold set: recovery follows it, not the
        // built-in default.
        let cfg =
            Config::parse(&format!("{SAMPLE}\nregression_threshold_pct = 5.0\n")).expect("parses");
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap()).unwrap();
        assert_eq!(settings.recovery_threshold_pct, 5.0);

        // Both set: an explicit lower recovery threshold is hysteresis.
        let cfg = Config::parse(&format!(
            "{SAMPLE}\nregression_threshold_pct = 10.0\nrecovery_threshold_pct = 5.0\n"
        ))
        .expect("parses");
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap()).unwrap();
        assert_eq!(settings.regression_threshold_pct, 10.0);
        assert_eq!(settings.recovery_threshold_pct, 5.0);
    }

    #[test]
    fn test_settings_rejects_out_of_range_values() {
        for bad in [
            "rolling_window = 0",
            "digest_batch_size = 0",
            "regression_threshold_pct = -5.0",
            "recovery_threshold_pct = 0.0",
            // Recovering above the regression trigger would flap.
            "regression_threshold_pct = 5.0\nrecovery_threshold_pct = 8.0",
            // The instructions pair follows the same validation.
            "instr_regression_threshold_pct = -1.0",
            "instr_recovery_threshold_pct = 0.0",
            "instr_regression_threshold_pct = 2.0\ninstr_recovery_threshold_pct = 3.0",
        ] {
            let cfg = Config::parse(&format!("{SAMPLE}\n{bad}\n")).expect("parses");
            assert!(
                cfg.settings(cfg.repo("mega-evm").unwrap()).is_err(),
                "'{bad}' should be rejected"
            );
        }
    }

    #[test]
    fn test_shipped_repos_toml_parses_and_resolves() {
        // The checked-in config must stay loadable: a syntax error or an
        // out-of-range knob would otherwise only surface on the prod box.
        let cfg = Config::parse(include_str!("../repos.toml")).expect("repos.toml parses");
        let repo = cfg.repo("mega-evm").expect("mega-evm entry");
        cfg.settings(repo).expect("settings resolve");
        // The instructions lane ships enabled for mega-evm, unfiltered and
        // best-effort (the require_instructions knob stays commented out).
        let instr = repo.instructions.as_ref().expect("instructions lane configured");
        assert_eq!(instr.bench_filter, None);
        assert!(!instr.require_instructions);
        assert!(repo.flamegraph.is_some(), "flamegraph config still parses next to it");
    }

    #[test]
    fn test_settings_defaults_section_and_per_repo_override() {
        let text = format!(
            "[defaults]\nregression_threshold_pct = 15.0\nrolling_window = 30\n\
             bench_profile = \"profiling\"\n\n{SAMPLE}\nregression_threshold_pct = 5.0\n\
             digest_batch_size = 5\n"
        );
        let cfg = Config::parse(&text).expect("parses");
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap()).unwrap();
        // Per-repo override wins.
        assert_eq!(settings.regression_threshold_pct, 5.0);
        assert_eq!(settings.digest_batch_size, 5);
        // [defaults] fills what the repo leaves unset.
        assert_eq!(settings.rolling_window, 30);
        assert_eq!(settings.bench_profile.as_deref(), Some("profiling"));
    }
}
