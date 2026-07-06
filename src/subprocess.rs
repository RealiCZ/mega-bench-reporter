//! Thin subprocess helpers shared by the git, bench, and flamegraph layers.

use std::process::{Child, Command};

/// Runs a command to completion and returns its trimmed stdout; a non-zero
/// exit becomes an error carrying the command's stderr.
pub fn run_cmd(cmd: &mut Command, what: &str) -> anyhow::Result<String> {
    let output =
        cmd.output().map_err(|e| anyhow::anyhow!("failed to spawn {what} ({cmd:?}): {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{what} failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Streams a child's piped stdout to our stderr — our own stdout is reserved
/// for the one JSON document each subcommand prints. If the drain breaks, the
/// child is killed and reaped so a running bench/profiler can't outlive the
/// failed run.
pub fn drain_stdout_to_stderr(child: &mut Child) -> anyhow::Result<()> {
    let mut stdout = child.stdout.take().expect("child stdout must be piped");
    if let Err(e) = std::io::copy(&mut stdout, &mut std::io::stderr()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn test_run_cmd_captures_stdout() {
        let out = run_cmd(Command::new("echo").arg("hello"), "echo").unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn test_run_cmd_nonzero_exit_is_error_with_context() {
        let err = run_cmd(&mut Command::new("false"), "false-cmd").unwrap_err();
        assert!(err.to_string().contains("false-cmd failed"));
    }

    #[test]
    fn test_drain_stdout_to_stderr_reaps_child() {
        let mut child = Command::new("echo")
            .arg("streamed")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn echo");
        drain_stdout_to_stderr(&mut child).unwrap();
        assert!(child.wait().unwrap().success());
    }
}
