use anyhow::{Context, Result};

use super::{GhClient, Issue};
use crate::process::CommandRunner;

impl<R: CommandRunner> GhClient<R> {
    /// Fetch open issues with the given label, ordered oldest first.
    pub async fn get_issues_by_label(&self, label: &str) -> Result<Vec<Issue>> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&[
                    "issue",
                    "list",
                    "--label",
                    label,
                    "--json",
                    "number,title,body,labels",
                    "--state",
                    "open",
                    "--limit",
                    "100",
                ]),
                &self.repo_dir,
            )
            .await
            .context("fetching issues by label")?;
        Self::check_output(&output, "fetch issues")?;

        let mut issues: Vec<Issue> =
            serde_json::from_str(&output.stdout).context("parsing issue list JSON")?;
        // gh returns newest first; we want oldest first (FIFO)
        issues.sort_by_key(|i| i.number);
        Ok(issues)
    }

    /// Fetch a single issue by number.
    pub async fn get_issue(&self, issue_number: u32) -> Result<Issue> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&[
                    "issue",
                    "view",
                    &issue_number.to_string(),
                    "--json",
                    "number,title,body,labels",
                ]),
                &self.repo_dir,
            )
            .await
            .context("fetching issue")?;
        Self::check_output(&output, "fetch issue")?;

        let issue: Issue = serde_json::from_str(&output.stdout).context("parsing issue JSON")?;
        Ok(issue)
    }

    /// Post a comment on an issue.
    pub async fn comment_on_issue(&self, issue_number: u32, body: &str) -> Result<()> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&["issue", "comment", &issue_number.to_string(), "--body", body]),
                &self.repo_dir,
            )
            .await
            .context("commenting on issue")?;
        Self::check_output(&output, "comment on issue")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        github::GhClient,
        process::{CommandOutput, MockCommandRunner},
    };

    #[tokio::test]
    async fn get_issues_by_label_parses_json() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"[{"number":3,"title":"Third","body":"c","labels":[{"name":"o-ready"}]},{"number":1,"title":"First","body":"a","labels":[{"name":"o-ready"}]},{"number":2,"title":"Second","body":"b","labels":[{"name":"o-ready"}]}]"#.to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let issues = client.get_issues_by_label("o-ready").await.unwrap();

        assert_eq!(issues.len(), 3);
        // Should be sorted oldest first (by number)
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[1].number, 2);
        assert_eq!(issues[2].number, 3);
    }

    #[tokio::test]
    async fn get_issue_parses_single() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"{"number":42,"title":"Fix bug","body":"details","labels":[]}"#
                        .to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let issue = client.get_issue(42).await.unwrap();

        assert_eq!(issue.number, 42);
        assert_eq!(issue.title, "Fix bug");
        assert_eq!(issue.body, "details");
    }

    #[tokio::test]
    async fn comment_on_issue_succeeds() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.comment_on_issue(42, "hello").await;
        assert!(result.is_ok());
    }
}
