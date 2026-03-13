use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::{IssueOrigin, IssueProvider, PipelineIssue};
use crate::{
    github::{self, GhClient, issues::parse_issue_frontmatter},
    process::CommandRunner,
};

/// Wraps `GhClient` to implement `IssueProvider` for GitHub issues.
pub struct GithubIssueProvider<R: CommandRunner> {
    client: Arc<GhClient<R>>,
    target_field: String,
}

impl<R: CommandRunner> GithubIssueProvider<R> {
    pub fn new(client: Arc<GhClient<R>>, target_field: &str) -> Self {
        Self { client, target_field: target_field.to_string() }
    }
}

#[async_trait]
impl<R: CommandRunner + 'static> IssueProvider for GithubIssueProvider<R> {
    async fn get_ready_issues(&self, label: &str) -> Result<Vec<PipelineIssue>> {
        let issues = self.client.get_issues_by_label(label).await?;
        Ok(issues
            .into_iter()
            .map(|i| {
                let parsed = parse_issue_frontmatter(&i, &self.target_field);
                PipelineIssue {
                    number: i.number,
                    title: i.title,
                    body: parsed.body_without_frontmatter,
                    source: IssueOrigin::Github,
                    target_repo: parsed.target_repo,
                }
            })
            .collect())
    }

    async fn get_issue(&self, number: u32) -> Result<PipelineIssue> {
        let issue = self.client.get_issue(number).await?;
        let parsed = parse_issue_frontmatter(&issue, &self.target_field);
        Ok(PipelineIssue {
            number: issue.number,
            title: issue.title,
            body: parsed.body_without_frontmatter,
            source: IssueOrigin::Github,
            target_repo: parsed.target_repo,
        })
    }

    async fn transition(&self, number: u32, from: &str, to: &str) -> Result<()> {
        github::transition_issue(&self.client, number, from, to).await
    }

    async fn comment(&self, number: u32, body: &str) -> Result<()> {
        self.client.comment_on_issue(number, body).await
    }

    async fn close(&self, number: u32, comment: Option<&str>) -> Result<()> {
        self.client.close_issue(number, comment).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::process::{CommandOutput, MockCommandRunner};

    #[tokio::test]
    async fn get_ready_issues_maps_to_pipeline_issues() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"[{"number":1,"title":"Fix bug","body":"details","labels":[]}]"#
                        .to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = Arc::new(GhClient::new(mock, Path::new("/tmp")));
        let provider = GithubIssueProvider::new(client, "target_repo");
        let issues = provider.get_ready_issues("o-ready").await.unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[0].source, IssueOrigin::Github);
        assert!(issues[0].target_repo.is_none());
    }

    #[tokio::test]
    async fn get_ready_issues_extracts_target_repo() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: r#"[{"number":2,"title":"Multi","body":"---\ntarget_repo: api\n---\n\nBody","labels":[]}]"#
                        .to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });

        let client = Arc::new(GhClient::new(mock, Path::new("/tmp")));
        let provider = GithubIssueProvider::new(client, "target_repo");
        let issues = provider.get_ready_issues("o-ready").await.unwrap();

        assert_eq!(issues[0].target_repo.as_deref(), Some("api"));
        assert_eq!(issues[0].body, "Body");
    }

    #[tokio::test]
    async fn transition_delegates_to_gh_client() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });

        let client = Arc::new(GhClient::new(mock, Path::new("/tmp")));
        let provider = GithubIssueProvider::new(client, "target_repo");
        let result = provider.transition(1, "o-ready", "o-cooking").await;
        assert!(result.is_ok());
    }
}
