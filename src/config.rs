//! Repo-config list (D8): which repos are tracked, what to bench, headline spec.
//! One entry today (mega-evm); the list shape is what leaves room for more.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RepoConfig {
    pub name: String,
    pub github: String,
    pub branch: String,
    pub clone_url: String,
    pub bench_targets: Vec<String>,
    pub headline_spec: String,
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
}

impl RepoConfig {
    pub fn package(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.name)
    }
}

/// The tunable knobs, all optional at both the `[defaults]` and per-repo
/// level. Resolution order: repo value → `[defaults]` value → built-in.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Tuning {
    /// Alert when a headline row rises more than this % over its rolling
    /// median (built-in: 10).
    pub regression_threshold_pct: Option<f64>,
    /// How many healthy runs feed the rolling median (built-in: 20).
    pub rolling_window: Option<usize>,
    /// Commits per trend digest (built-in: 10).
    pub digest_batch_size: Option<u32>,
    /// Cargo profile for `cargo bench` runs. Unset = cargo's default bench
    /// profile — the same invocation mega-evm's CI uses. Set to `"profiling"`
    /// to bench the debug-symbol build the flamegraph pipeline uses.
    pub bench_profile: Option<String>,
}

/// Fully-resolved settings for one repo.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub regression_threshold_pct: f64,
    pub rolling_window: usize,
    pub digest_batch_size: u32,
    pub bench_profile: Option<String>,
}

impl Settings {
    fn resolve(repo: &Tuning, defaults: &Tuning) -> Self {
        Self {
            regression_threshold_pct: repo
                .regression_threshold_pct
                .or(defaults.regression_threshold_pct)
                .unwrap_or(crate::state::REGRESSION_THRESHOLD_PCT),
            rolling_window: repo
                .rolling_window
                .or(defaults.rolling_window)
                .unwrap_or(crate::state::ROLLING_WINDOW),
            digest_batch_size: repo
                .digest_batch_size
                .or(defaults.digest_batch_size)
                .unwrap_or(crate::state::DIGEST_BATCH_SIZE),
            bench_profile: repo.bench_profile.clone().or_else(|| defaults.bench_profile.clone()),
        }
    }
}

/// Nightly flame-graph settings (D6). The workload set is config, not code, so
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
    /// over built-ins.
    pub fn settings(&self, repo: &RepoConfig) -> Settings {
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
headline_spec = "rex5"
"#;

    #[test]
    fn test_parse_single_repo() {
        let cfg = Config::parse(SAMPLE).expect("parses");
        assert_eq!(cfg.repos.len(), 1);
        let repo = cfg.repo("mega-evm").expect("found");
        assert_eq!(repo.github, "megaeth-labs/mega-evm");
        assert_eq!(repo.branch, "main");
        assert_eq!(repo.headline_spec, "rex5");
        assert_eq!(repo.bench_targets.len(), 5);
    }

    #[test]
    fn test_parse_multiple_repos_is_a_list_not_a_special_case() {
        let two = format!(
            "{SAMPLE}\n[[repos]]\nname = \"mega-reth\"\ngithub = \"megaeth-labs/mega-reth\"\n\
             branch = \"main\"\nclone_url = \"git@github.com:megaeth-labs/mega-reth.git\"\n\
             bench_targets = []\nheadline_spec = \"rex5\"\n"
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
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap());
        assert_eq!(settings.regression_threshold_pct, crate::state::REGRESSION_THRESHOLD_PCT);
        assert_eq!(settings.rolling_window, crate::state::ROLLING_WINDOW);
        assert_eq!(settings.digest_batch_size, crate::state::DIGEST_BATCH_SIZE);
        assert_eq!(settings.bench_profile, None);
    }

    #[test]
    fn test_settings_defaults_section_and_per_repo_override() {
        let text = format!(
            "[defaults]\nregression_threshold_pct = 15.0\nrolling_window = 30\n\
             bench_profile = \"profiling\"\n\n{SAMPLE}\nregression_threshold_pct = 5.0\n\
             digest_batch_size = 5\n"
        );
        let cfg = Config::parse(&text).expect("parses");
        let settings = cfg.settings(cfg.repo("mega-evm").unwrap());
        // Per-repo override wins.
        assert_eq!(settings.regression_threshold_pct, 5.0);
        assert_eq!(settings.digest_batch_size, 5);
        // [defaults] fills what the repo leaves unset.
        assert_eq!(settings.rolling_window, 30);
        assert_eq!(settings.bench_profile.as_deref(), Some("profiling"));
    }
}
