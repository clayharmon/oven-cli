pub mod issues;
pub mod labels;
pub mod prs;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::process::{CommandOutput, CommandRunner};

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

/// Transition an issue from one label to another.
pub async fn transition_issue<R: CommandRunner>(
    client: &GhClient<R>,
    issue_number: u32,
    from: &str,
    to: &str,
) -> Result<()> {
    client.remove_label(issue_number, from).await?;
    client.add_label(issue_number, to).await?;
    Ok(())
}

/// Post a comment, logging errors instead of propagating them.
///
/// Comment failures should never crash the pipeline.
pub async fn safe_comment<R: CommandRunner>(client: &GhClient<R>, pr_number: u32, body: &str) {
    if let Err(e) = client.comment_on_pr(pr_number, body).await {
        tracing::warn!("failed to post comment on PR #{pr_number}: {e}");
    }
}
