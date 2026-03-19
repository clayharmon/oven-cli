pub mod stream;

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use tokio::{io::AsyncWriteExt, process::Command};
use tracing::warn;

use self::stream::parse_stream;
use crate::agents::AgentInvocation;

const MAX_RETRIES: u32 = 2;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(15)];
const TRANSIENT_PATTERNS: &[&str] = &[
    "connection reset",
    "connection refused",
    "timed out",
    "timeout",
    "rate limit",
    "rate_limit",
    "502",
    "503",
    "429",
    "overloaded",
    "econnrefused",
];

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
        max_turns: Option<u32>,
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
        max_turns: Option<u32>,
    ) -> Result<AgentResult> {
        let tools_arg = allowed_tools.join(",");

        let mut cmd = Command::new("claude");
        cmd.args(["-p", "--verbose", "--output-format", "stream-json"])
            .args(["--allowedTools", &tools_arg]);

        if let Some(turns) = max_turns {
            cmd.args(["--max-turns", &turns.to_string()]);
        }

        let mut child = cmd
            .current_dir(working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning claude")?;

        // Pass prompt via stdin to avoid leaking it in process listings (ps aux).
        let mut stdin = child.stdin.take().context("capturing claude stdin")?;
        stdin.write_all(prompt.as_bytes()).await.context("writing prompt to claude stdin")?;
        stdin.shutdown().await.context("closing claude stdin")?;
        drop(stdin);

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

/// Check whether an error message indicates a transient failure worth retrying.
pub fn is_transient_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    TRANSIENT_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Invoke an agent with retry logic for transient failures.
///
/// Retries up to `MAX_RETRIES` times (with backoff) when the error message
/// matches known transient patterns (connection resets, rate limits, 5xx, etc.).
pub async fn run_with_retry<R: CommandRunner>(
    runner: &R,
    invocation: &AgentInvocation,
) -> Result<AgentResult> {
    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        match crate::agents::invoke_agent(runner, invocation).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                let msg = format!("{e:#}");
                if attempt < MAX_RETRIES && is_transient_error(&msg) {
                    let delay = RETRY_DELAYS[attempt as usize];
                    warn!(
                        attempt = attempt + 1,
                        max = MAX_RETRIES,
                        delay_secs = delay.as_secs(),
                        error = %msg,
                        "transient agent failure, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    last_err = Some(e);
                } else {
                    return Err(e);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("agent invocation failed after retries")))
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

    #[test]
    fn transient_error_detection() {
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error("Connection Refused"));
        assert!(is_transient_error("request timed out after 30s"));
        assert!(is_transient_error("rate limit exceeded"));
        assert!(is_transient_error("rate_limit_error"));
        assert!(is_transient_error("HTTP 502 Bad Gateway"));
        assert!(is_transient_error("Service Unavailable (503)"));
        assert!(is_transient_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_error("server is overloaded"));
        assert!(is_transient_error("ECONNREFUSED 127.0.0.1:443"));
    }

    #[test]
    fn non_transient_errors_not_matched() {
        assert!(!is_transient_error("file not found"));
        assert!(!is_transient_error("permission denied"));
        assert!(!is_transient_error("invalid JSON in response"));
        assert!(!is_transient_error("authentication failed"));
        assert!(!is_transient_error(""));
    }
}
