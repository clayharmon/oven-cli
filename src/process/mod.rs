pub mod stream;

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use tokio::process::Command;

use self::stream::parse_stream;

/// Result from a Claude agent invocation.
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub cost_usd: f64,
    pub duration: Duration,
    pub turns: u32,
    pub output: String,
    pub session_id: String,
    pub success: bool,
}

/// Result from a simple command execution (e.g., gh CLI).
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Trait for running external commands.
///
/// Enables mocking in tests so we never call real CLIs.
/// Uses `String` slices rather than `&str` slices for mockall compatibility.
#[cfg_attr(test, mockall::automock)]
pub trait CommandRunner: Send + Sync {
    fn run_claude(
        &self,
        prompt: &str,
        allowed_tools: &[String],
        working_dir: &Path,
    ) -> impl std::future::Future<Output = Result<AgentResult>> + Send;

    fn run_gh(
        &self,
        args: &[String],
        working_dir: &Path,
    ) -> impl std::future::Future<Output = Result<CommandOutput>> + Send;
}

/// Real implementation that spawns actual subprocesses.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    async fn run_claude(
        &self,
        prompt: &str,
        allowed_tools: &[String],
        working_dir: &Path,
    ) -> Result<AgentResult> {
        let tools_arg = allowed_tools.join(",");

        let mut child = Command::new("claude")
            .args(["-p", "--output-format", "stream-json"])
            .args(["--allowedTools", &tools_arg])
            .arg("--")
            .arg(prompt)
            .current_dir(working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning claude")?;

        let stdout = child.stdout.take().context("capturing claude stdout")?;
        let result = parse_stream(stdout).await?;
        let status = child.wait().await.context("waiting for claude")?;

        Ok(AgentResult {
            cost_usd: result.cost_usd,
            duration: result.duration,
            turns: result.turns,
            output: result.output,
            session_id: result.session_id,
            success: status.success(),
        })
    }

    async fn run_gh(&self, args: &[String], working_dir: &Path) -> Result<CommandOutput> {
        let output = Command::new("gh")
            .args(args)
            .current_dir(working_dir)
            .kill_on_drop(true)
            .output()
            .await
            .context("spawning gh")?;

        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_result_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AgentResult>();
        assert_send_sync::<CommandOutput>();
    }

    #[test]
    fn real_command_runner_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RealCommandRunner>();
    }
}
