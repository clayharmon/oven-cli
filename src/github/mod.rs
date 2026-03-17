pub mod issues;
pub mod labels;
pub mod prs;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::process::{CommandOutput, CommandRunner};

/// The merge state of a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// A GitHub issue.
#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u32,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
}

/// A label on a GitHub issue (gh CLI returns objects with a `name` field).
#[derive(Debug, Clone, Deserialize)]
pub struct IssueLabel {
    pub name: String,
}

/// Client for GitHub operations via the `gh` CLI.
pub struct GhClient<R: CommandRunner> {
    runner: R,
    repo_dir: PathBuf,
}

impl<R: CommandRunner> GhClient<R> {
    pub fn new(runner: R, repo_dir: &Path) -> Self {
        Self { runner, repo_dir: repo_dir.to_path_buf() }
    }

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    fn check_output(output: &CommandOutput, operation: &str) -> Result<()> {
        if !output.success {
            anyhow::bail!("{operation} failed: {}", output.stderr.trim());
        }
        Ok(())
    }
}

/// Transition an issue from one label to another in a single gh call.
pub async fn transition_issue<R: CommandRunner>(
    client: &GhClient<R>,
    issue_number: u32,
    from: &str,
    to: &str,
) -> Result<()> {
    client.swap_labels(issue_number, from, to).await
}

/// Post a comment on a PR in a specific repo, logging errors instead of propagating them.
///
/// Comment failures should never crash the pipeline.
pub async fn safe_comment<R: CommandRunner>(
    client: &GhClient<R>,
    pr_number: u32,
    body: &str,
    repo_dir: &Path,
) {
    if let Err(e) = client.comment_on_pr_in(pr_number, body, repo_dir).await {
        tracing::warn!("failed to post comment on PR #{pr_number}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CommandOutput, MockCommandRunner};

    fn mock_gh_success() -> MockCommandRunner {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });
        mock
    }

    fn mock_gh_failure() -> MockCommandRunner {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: "API error".to_string(),
                    success: false,
                })
            })
        });
        mock
    }

    #[tokio::test]
    async fn transition_issue_removes_and_adds_labels() {
        let client = GhClient::new(mock_gh_success(), std::path::Path::new("/tmp"));
        let result = transition_issue(&client, 42, "o-ready", "o-cooking").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn safe_comment_swallows_errors() {
        let client = GhClient::new(mock_gh_failure(), std::path::Path::new("/tmp"));
        safe_comment(&client, 42, "test comment", std::path::Path::new("/tmp")).await;
    }

    #[tokio::test]
    async fn safe_comment_succeeds_on_success() {
        let client = GhClient::new(mock_gh_success(), std::path::Path::new("/tmp"));
        safe_comment(&client, 42, "test comment", std::path::Path::new("/tmp")).await;
    }

    #[tokio::test]
    async fn safe_comment_uses_given_repo_dir() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, dir| {
            assert_eq!(dir, std::path::Path::new("/repos/target"));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });
        let client = GhClient::new(mock, std::path::Path::new("/repos/god"));
        safe_comment(&client, 42, "test", std::path::Path::new("/repos/target")).await;
    }

    #[test]
    fn check_output_returns_error_on_failure() {
        let output = CommandOutput {
            stdout: String::new(),
            stderr: "not found".to_string(),
            success: false,
        };
        let result = GhClient::<MockCommandRunner>::check_output(&output, "test op");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn check_output_ok_on_success() {
        let output =
            CommandOutput { stdout: "ok".to_string(), stderr: String::new(), success: true };
        let result = GhClient::<MockCommandRunner>::check_output(&output, "test op");
        assert!(result.is_ok());
    }
}
