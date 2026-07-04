//! Git checkout management for tracked repos: clone/fetch (with network
//! retry and optional token auth), detached checkouts, and commit metadata.
//! Everything network-facing lives here; the bench/post-processing pipeline
//! never talks to git directly.

use crate::config::RepoConfig;
use crate::subprocess::run_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Commit-level metadata stored in `raw.json`.
#[derive(Debug, Clone)]
pub struct CommitMeta {
    /// Full commit sha.
    pub sha: String,
    /// Committer date, RFC3339.
    pub date: String,
    /// `rustc --version` output.
    pub rustc: String,
}

fn git(checkout: &Path, args: &[&str], what: &str) -> anyhow::Result<String> {
    run_cmd(Command::new("git").arg("-C").arg(checkout).args(args), what)
}

/// Retries a git operation that talks to the network (clone / fetch /
/// submodule update) with backoff. Transient failures to reach the remote
/// (observed in the wild: SSL connection timeouts to GitHub) shouldn't fail
/// an entire bench run. `backoff_secs` has one entry per retry.
fn with_network_retry<T>(
    what: &str,
    backoff_secs: &[u64],
    mut op: impl FnMut() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut attempt = 0;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if attempt < backoff_secs.len() => {
                let wait = backoff_secs[attempt];
                attempt += 1;
                eprintln!(
                    "{what} failed (attempt {attempt}/{}, retrying in {wait}s): {e:#}",
                    backoff_secs.len() + 1
                );
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Default backoff for git network operations: two retries.
const GIT_RETRY_BACKOFF_SECS: &[u64] = &[5, 15];

/// A `credential.helper` snippet that feeds `$GITHUB_TOKEN` from the process
/// environment to git for https remotes — the token never appears in argv, and
/// without the env var set git falls back to anonymous access (fine for public
/// repos). The invoking agent populates the env var when needed.
const TOKEN_CREDENTIAL_HELPER: &str =
    "!f() { echo username=x-access-token; echo \"password=${GITHUB_TOKEN}\"; }; f";

fn git_credential_args(clone_url: &str) -> Vec<String> {
    credential_args_for(clone_url, std::env::var("GITHUB_TOKEN").is_ok())
}

/// The env-independent core of [`git_credential_args`], testable without
/// mutating the process environment.
fn credential_args_for(clone_url: &str, has_token: bool) -> Vec<String> {
    if clone_url.starts_with("https://") && has_token {
        vec!["-c".into(), format!("credential.helper={TOKEN_CREDENTIAL_HELPER}")]
    } else {
        Vec::new()
    }
}

/// Clones (first run) or reuses the tracked repo's checkout under
/// `<work_root>/<repo name>`. Submodules included — mega-evm needs them.
/// A leftover directory without `.git` (an interrupted first clone) is
/// removed and re-cloned instead of wedging every subsequent run — the
/// checkout dir is fully machine-managed scratch.
pub fn ensure_checkout(work_root: &Path, repo: &RepoConfig) -> anyhow::Result<PathBuf> {
    let checkout = work_root.join(&repo.name);
    if checkout.join(".git").exists() {
        return Ok(checkout);
    }
    std::fs::create_dir_all(work_root)?;
    with_network_retry(&format!("git clone {}", repo.clone_url), GIT_RETRY_BACKOFF_SECS, || {
        // A leftover from an interrupted/failed previous attempt would make
        // `git clone` refuse with "destination exists" — clear it first.
        if checkout.exists() {
            eprintln!("removing broken checkout {} (no .git) and re-cloning", checkout.display());
            std::fs::remove_dir_all(&checkout)?;
        }
        let mut cmd = Command::new("git");
        cmd.args(git_credential_args(&repo.clone_url));
        cmd.arg("clone").arg("--recursive").arg(&repo.clone_url).arg(&checkout);
        run_cmd(&mut cmd, "git clone")?;
        Ok(())
    })?;
    Ok(checkout)
}

/// Fetches the tracked branch and checks out `sha` (detached), updating
/// submodules. `--force` so tracked files rewritten by a previous bench run
/// (classic: a regenerated `Cargo.lock`) can't wedge the checkout. Falls back
/// to fetching the sha directly if the branch fetch didn't make it reachable
/// (e.g. a force-pushed branch).
pub fn checkout_commit(checkout: &Path, repo: &RepoConfig, sha: &str) -> anyhow::Result<()> {
    let cred = git_credential_args(&repo.clone_url);
    let cred_refs: Vec<&str> = cred.iter().map(String::as_str).collect();

    let fetch_branch: Vec<&str> = [&cred_refs[..], &["fetch", "origin", &repo.branch]].concat();
    with_network_retry("git fetch", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &fetch_branch, "git fetch").map(|_| ())
    })?;
    if git(checkout, &["checkout", "--force", "--detach", sha], "git checkout").is_err() {
        let fetch_sha: Vec<&str> = [&cred_refs[..], &["fetch", "origin", sha]].concat();
        with_network_retry("git fetch <sha>", GIT_RETRY_BACKOFF_SECS, || {
            git(checkout, &fetch_sha, "git fetch <sha>").map(|_| ())
        })?;
        git(checkout, &["checkout", "--force", "--detach", sha], "git checkout")?;
    }
    update_submodules(checkout, &cred_refs)?;
    Ok(())
}

/// Fetches the tracked branch and checks out its remote HEAD (detached). Used
/// by the nightly flamegraph pipeline, which profiles "current main", not a
/// specific commit.
pub fn checkout_branch_head(checkout: &Path, repo: &RepoConfig) -> anyhow::Result<()> {
    let cred = git_credential_args(&repo.clone_url);
    let cred_refs: Vec<&str> = cred.iter().map(String::as_str).collect();
    let fetch: Vec<&str> = [&cred_refs[..], &["fetch", "origin", &repo.branch]].concat();
    with_network_retry("git fetch", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &fetch, "git fetch").map(|_| ())
    })?;
    git(checkout, &["checkout", "--force", "--detach", "FETCH_HEAD"], "git checkout FETCH_HEAD")?;
    update_submodules(checkout, &cred_refs)?;
    Ok(())
}

fn update_submodules(checkout: &Path, cred_refs: &[&str]) -> anyhow::Result<()> {
    let update_subs: Vec<&str> =
        [cred_refs, &["submodule", "update", "--init", "--recursive"]].concat();
    with_network_retry("git submodule update", GIT_RETRY_BACKOFF_SECS, || {
        git(checkout, &update_subs, "git submodule update").map(|_| ())
    })?;
    Ok(())
}

/// The checkout's current HEAD sha.
pub fn head_sha(checkout: &Path) -> anyhow::Result<String> {
    git(checkout, &["rev-parse", "HEAD"], "git rev-parse HEAD")
}

/// Commit metadata straight from the checkout: committer date + rustc version
/// (run inside the checkout so a `rust-toolchain.toml` is honored).
pub fn commit_meta(checkout: &Path, sha: &str) -> anyhow::Result<CommitMeta> {
    let date = git(checkout, &["show", "-s", "--format=%cI", sha], "git show")?;
    let rustc =
        run_cmd(Command::new("rustc").arg("--version").current_dir(checkout), "rustc --version")?;
    Ok(CommitMeta { sha: sha.to_string(), date, rustc })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_network_retry_retries_then_succeeds() {
        let mut attempts = 0;
        let result = with_network_retry("op", &[0, 0], || {
            attempts += 1;
            if attempts < 3 {
                anyhow::bail!("transient");
            }
            Ok(attempts)
        })
        .unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn test_with_network_retry_gives_up_after_backoff_exhausted() {
        let mut attempts = 0;
        let result: anyhow::Result<()> = with_network_retry("op", &[0], || {
            attempts += 1;
            anyhow::bail!("still down")
        });
        assert!(result.is_err());
        assert_eq!(attempts, 2, "one initial try + one retry");
    }

    #[test]
    fn test_credential_args_only_for_https_with_token() {
        let https = "https://github.com/megaeth-labs/mega-evm.git";
        let ssh = "git@github.com:megaeth-labs/mega-evm.git";
        assert_eq!(credential_args_for(https, true).len(), 2);
        assert!(credential_args_for(https, false).is_empty());
        assert!(credential_args_for(ssh, true).is_empty());
        assert!(credential_args_for(ssh, false).is_empty());
        // The token itself must never appear in argv — only the helper snippet
        // that reads it from the environment.
        assert!(credential_args_for(https, true).iter().all(|a| !a.contains("ghp_")));
    }
}
