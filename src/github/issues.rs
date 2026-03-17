use anyhow::{Context, Result};

use super::{GhClient, Issue};
use crate::process::CommandRunner;

/// An issue with parsed frontmatter metadata.
///
/// When multi-repo mode is enabled, issues can include YAML frontmatter at the
/// top of the body to specify which target repo the work should go to.
#[derive(Debug, Clone)]
pub struct ParsedIssue {
    pub issue: Issue,
    pub target_repo: Option<String>,
    pub body_without_frontmatter: String,
}

/// Parse YAML frontmatter from an issue body, extracting the target repo field.
///
/// Frontmatter is delimited by `---` on its own line at the very start of the body.
/// Only the field named by `target_field` is extracted; all other frontmatter is ignored.
/// The returned `body_without_frontmatter` has the frontmatter block stripped.
pub fn parse_issue_frontmatter(issue: &Issue, target_field: &str) -> ParsedIssue {
    let body = issue.body.trim_start();

    if !body.starts_with("---") {
        return ParsedIssue {
            issue: issue.clone(),
            target_repo: None,
            body_without_frontmatter: issue.body.clone(),
        };
    }

    // Find the closing --- delimiter (skip the opening one)
    let after_open = &body[3..];
    let closing = after_open.find("\n---");

    let Some(close_idx) = closing else {
        // No closing delimiter -- treat entire body as content (not frontmatter)
        return ParsedIssue {
            issue: issue.clone(),
            target_repo: None,
            body_without_frontmatter: issue.body.clone(),
        };
    };

    let frontmatter = &after_open[..close_idx];
    let rest = &after_open[close_idx + 4..]; // skip "\n---"
    let body_without = rest.trim_start_matches('\n').to_string();

    // Simple key: value extraction (no full YAML parser needed)
    let needle = format!("{target_field}:");
    let target_repo = frontmatter.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with(&needle) {
            Some(trimmed[needle.len()..].trim().to_string())
        } else {
            None
        }
    });

    ParsedIssue { issue: issue.clone(), target_repo, body_without_frontmatter: body_without }
}

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
                    "--author",
                    "@me",
                    "--json",
                    "number,title,body,labels,author",
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
                    "number,title,body,labels,author",
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

    /// Close an issue with an optional comment.
    pub async fn close_issue(&self, issue_number: u32, comment: Option<&str>) -> Result<()> {
        let num_str = issue_number.to_string();
        let mut args = vec!["issue", "close", &num_str];
        if let Some(body) = comment {
            args.extend(["--comment", body]);
        }
        let output =
            self.runner.run_gh(&Self::s(&args), &self.repo_dir).await.context("closing issue")?;
        Self::check_output(&output, "close issue")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
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
    async fn get_issues_by_label_filters_by_current_user() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|args, _| {
            assert!(args.contains(&"--author".to_string()));
            assert!(args.contains(&"@me".to_string()));
            Box::pin(async {
                Ok(CommandOutput { stdout: "[]".to_string(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let issues = client.get_issues_by_label("o-ready").await.unwrap();
        assert!(issues.is_empty());
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

    #[tokio::test]
    async fn close_issue_with_comment() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|args, _| {
            assert!(args.contains(&"issue".to_string()));
            assert!(args.contains(&"close".to_string()));
            assert!(args.contains(&"--comment".to_string()));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.close_issue(42, Some("Done")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn close_issue_without_comment() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|args, _| {
            assert!(!args.contains(&"--comment".to_string()));
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.close_issue(42, None).await;
        assert!(result.is_ok());
    }

    fn make_issue(body: &str) -> Issue {
        Issue {
            number: 1,
            title: "Test".to_string(),
            body: body.to_string(),
            labels: vec![],
            author: None,
        }
    }

    #[test]
    fn parse_frontmatter_extracts_target_repo() {
        let issue = make_issue("---\ntarget_repo: my-service\n---\n\nFix the bug");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert_eq!(parsed.target_repo.as_deref(), Some("my-service"));
        assert_eq!(parsed.body_without_frontmatter, "Fix the bug");
    }

    #[test]
    fn parse_frontmatter_custom_field_name() {
        let issue = make_issue("---\nrepo: other-thing\n---\n\nDo stuff");
        let parsed = parse_issue_frontmatter(&issue, "repo");
        assert_eq!(parsed.target_repo.as_deref(), Some("other-thing"));
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let issue = make_issue("Just a regular issue body");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert!(parsed.target_repo.is_none());
        assert_eq!(parsed.body_without_frontmatter, "Just a regular issue body");
    }

    #[test]
    fn parse_frontmatter_unclosed_delimiters() {
        let issue = make_issue("---\ntarget_repo: oops\nno closing delimiter");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert!(parsed.target_repo.is_none());
        assert_eq!(parsed.body_without_frontmatter, issue.body);
    }

    #[test]
    fn parse_frontmatter_missing_field() {
        let issue = make_issue("---\nother_key: value\n---\n\nBody here");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert!(parsed.target_repo.is_none());
        assert_eq!(parsed.body_without_frontmatter, "Body here");
    }

    #[test]
    fn parse_frontmatter_strips_leading_newlines() {
        let issue = make_issue("---\ntarget_repo: svc\n---\n\n\nBody");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert_eq!(parsed.body_without_frontmatter, "Body");
    }

    #[test]
    fn parse_frontmatter_preserves_issue() {
        let issue = make_issue("---\ntarget_repo: api\n---\nContent");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert_eq!(parsed.issue.number, 1);
        assert_eq!(parsed.issue.title, "Test");
    }

    #[test]
    fn parse_frontmatter_with_extra_fields() {
        let issue =
            make_issue("---\npriority: high\ntarget_repo: backend\nlabel: bug\n---\n\nDetails");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert_eq!(parsed.target_repo.as_deref(), Some("backend"));
        assert_eq!(parsed.body_without_frontmatter, "Details");
    }

    #[test]
    fn parse_frontmatter_empty_body() {
        let issue = make_issue("");
        let parsed = parse_issue_frontmatter(&issue, "target_repo");
        assert!(parsed.target_repo.is_none());
        assert_eq!(parsed.body_without_frontmatter, "");
    }
}
