use std::path::Path;

use anyhow::{Context, Result};
use tracing::warn;

use super::{GhClient, PrState};
use crate::{config::MergeStrategy, process::CommandRunner};

impl<R: CommandRunner> GhClient<R> {
    /// Create a draft pull request and return its number.
    pub async fn create_draft_pr(&self, title: &str, branch: &str, body: &str) -> Result<u32> {
        self.create_draft_pr_in(title, branch, body, &self.repo_dir).await
    }

    /// Create a draft pull request in a specific repo directory and return its number.
    ///
    /// Used in multi-repo mode where the PR belongs in the target repo, not the god repo.
    pub async fn create_draft_pr_in(
        &self,
        title: &str,
        branch: &str,
        body: &str,
        repo_dir: &Path,
    ) -> Result<u32> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&[
                    "pr", "create", "--title", title, "--body", body, "--head", branch, "--draft",
                ]),
                repo_dir,
            )
            .await
            .context("creating draft PR")?;
        Self::check_output(&output, "create draft PR")?;

        // gh pr create outputs the PR URL; extract the number from it
        let url = output.stdout.trim();
        let pr_number = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .context("parsing PR number from gh output")?;

        Ok(pr_number)
    }

    /// Post a comment on a pull request (in the default repo).
    pub async fn comment_on_pr(&self, pr_number: u32, body: &str) -> Result<()> {
        self.comment_on_pr_in(pr_number, body, &self.repo_dir).await
    }

    /// Post a comment on a pull request in a specific repo directory.
    pub async fn comment_on_pr_in(
        &self,
        pr_number: u32,
        body: &str,
        repo_dir: &Path,
    ) -> Result<()> {
        let output = self
            .runner
            .run_gh(&Self::s(&["pr", "comment", &pr_number.to_string(), "--body", body]), repo_dir)
            .await
            .context("commenting on PR")?;
        Self::check_output(&output, "comment on PR")?;
        Ok(())
    }

    /// Update the title and body of a pull request (in the default repo).
    pub async fn edit_pr(&self, pr_number: u32, title: &str, body: &str) -> Result<()> {
        self.edit_pr_in(pr_number, title, body, &self.repo_dir).await
    }

    /// Update the title and body of a pull request in a specific repo directory.
    pub async fn edit_pr_in(
        &self,
        pr_number: u32,
        title: &str,
        body: &str,
        repo_dir: &Path,
    ) -> Result<()> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&["pr", "edit", &pr_number.to_string(), "--title", title, "--body", body]),
                repo_dir,
            )
            .await
            .context("editing PR")?;
        Self::check_output(&output, "edit PR")?;
        Ok(())
    }

    /// Mark a PR as ready for review (in the default repo).
    pub async fn mark_pr_ready(&self, pr_number: u32) -> Result<()> {
        self.mark_pr_ready_in(pr_number, &self.repo_dir).await
    }

    /// Mark a PR as ready for review in a specific repo directory.
    pub async fn mark_pr_ready_in(&self, pr_number: u32, repo_dir: &Path) -> Result<()> {
        let output = self
            .runner
            .run_gh(&Self::s(&["pr", "ready", &pr_number.to_string()]), repo_dir)
            .await
            .context("marking PR ready")?;
        Self::check_output(&output, "mark PR ready")?;
        Ok(())
    }

    /// Check the merge state of a pull request (in the default repo).
    pub async fn get_pr_state(&self, pr_number: u32) -> Result<PrState> {
        self.get_pr_state_in(pr_number, &self.repo_dir).await
    }

    /// Check the merge state of a pull request in a specific repo directory.
    pub async fn get_pr_state_in(&self, pr_number: u32, repo_dir: &Path) -> Result<PrState> {
        let output = self
            .runner
            .run_gh(&Self::s(&["pr", "view", &pr_number.to_string(), "--json", "state"]), repo_dir)
            .await
            .context("checking PR state")?;
        Self::check_output(&output, "check PR state")?;

        let parsed: serde_json::Value =
            serde_json::from_str(output.stdout.trim()).context("parsing PR state JSON")?;
        let state_str = parsed["state"].as_str().unwrap_or("UNKNOWN");

        Ok(match state_str {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            "OPEN" => PrState::Open,
            other => {
                warn!(pr = pr_number, state = other, "unexpected PR state, treating as Open");
                PrState::Open
            }
        })
    }

    /// Merge a pull request (in the default repo).
    pub async fn merge_pr(&self, pr_number: u32, strategy: &MergeStrategy) -> Result<()> {
        self.merge_pr_in(pr_number, strategy, &self.repo_dir).await
    }

    /// Merge a pull request in a specific repo directory.
    pub async fn merge_pr_in(
        &self,
        pr_number: u32,
        strategy: &MergeStrategy,
        repo_dir: &Path,
    ) -> Result<()> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&["pr", "merge", &pr_number.to_string(), strategy.gh_flag()]),
                repo_dir,
            )
            .await
            .context("merging PR")?;
        Self::check_output(&output, "merge PR")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        config::MergeStrategy,
        github::GhClient,
        process::{CommandOutput, MockCommandRunner},
    };

    #[tokio::test]
    async fn create_draft_pr_returns_number() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: "https://github.com/user/repo/pull/99\n".to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let pr_number = client.create_draft_pr("title", "branch", "body").await.unwrap();
        assert_eq!(pr_number, 99);
    }

    #[tokio::test]
    async fn edit_pr_succeeds() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.edit_pr(42, "new title", "new body").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn edit_pr_in_uses_given_dir() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, dir| {
            assert_eq!(dir, Path::new("/repos/backend"));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/repos/god"));
        let result = client.edit_pr_in(42, "title", "body", Path::new("/repos/backend")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn edit_pr_failure_propagates() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: "not found".to_string(),
                    success: false,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.edit_pr(42, "title", "body").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn comment_on_pr_succeeds() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.comment_on_pr(42, "looks good").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn mark_pr_ready_succeeds() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.mark_pr_ready(42).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn merge_pr_succeeds() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.merge_pr(42, &MergeStrategy::Squash).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn get_pr_state_merged() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"{"state":"MERGED"}"#.to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let state = client.get_pr_state(42).await.unwrap();
        assert_eq!(state, crate::github::PrState::Merged);
    }

    #[tokio::test]
    async fn get_pr_state_open() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"{"state":"OPEN"}"#.to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let state = client.get_pr_state(42).await.unwrap();
        assert_eq!(state, crate::github::PrState::Open);
    }

    #[tokio::test]
    async fn get_pr_state_closed() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"{"state":"CLOSED"}"#.to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let state = client.get_pr_state(42).await.unwrap();
        assert_eq!(state, crate::github::PrState::Closed);
    }

    #[tokio::test]
    async fn get_pr_state_unknown_defaults_to_open() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"{"state":"DRAFT"}"#.to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let state = client.get_pr_state(42).await.unwrap();
        assert_eq!(state, crate::github::PrState::Open);
    }

    #[tokio::test]
    async fn merge_pr_failure_propagates() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: "merge conflict".to_string(),
                    success: false,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.merge_pr(42, &MergeStrategy::Squash).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("merge conflict"));
    }

    #[tokio::test]
    async fn comment_on_pr_in_uses_given_dir() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, dir| {
            assert_eq!(dir, Path::new("/repos/backend"));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/repos/god"));
        let result = client.comment_on_pr_in(42, "comment", Path::new("/repos/backend")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn get_pr_state_in_uses_given_dir() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, dir| {
            assert_eq!(dir, Path::new("/repos/backend"));
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"{"state":"MERGED"}"#.to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/repos/god"));
        let state = client.get_pr_state_in(42, Path::new("/repos/backend")).await.unwrap();
        assert_eq!(state, crate::github::PrState::Merged);
    }

    #[tokio::test]
    async fn mark_pr_ready_in_uses_given_dir() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, dir| {
            assert_eq!(dir, Path::new("/repos/backend"));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/repos/god"));
        let result = client.mark_pr_ready_in(42, Path::new("/repos/backend")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn merge_pr_in_uses_given_dir() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, dir| {
            assert_eq!(dir, Path::new("/repos/backend"));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/repos/god"));
        let result =
            client.merge_pr_in(42, &MergeStrategy::Squash, Path::new("/repos/backend")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn merge_pr_passes_squash_flag() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|args, _| {
            assert!(args.contains(&"--squash".to_string()), "expected --squash in {args:?}");
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        client.merge_pr(42, &MergeStrategy::Squash).await.unwrap();
    }

    #[tokio::test]
    async fn merge_pr_passes_merge_flag() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|args, _| {
            assert!(args.contains(&"--merge".to_string()), "expected --merge in {args:?}");
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        client.merge_pr(42, &MergeStrategy::Merge).await.unwrap();
    }

    #[tokio::test]
    async fn merge_pr_passes_rebase_flag() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|args, _| {
            assert!(args.contains(&"--rebase".to_string()), "expected --rebase in {args:?}");
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        client.merge_pr(42, &MergeStrategy::Rebase).await.unwrap();
    }
}
