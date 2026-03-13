pub mod github;
pub mod local;

use anyhow::Result;
use async_trait::async_trait;

/// A normalized issue from any source.
///
/// Both GitHub and local issues are converted to this struct before
/// entering the pipeline. This keeps the pipeline source-agnostic.
#[derive(Debug, Clone)]
pub struct PipelineIssue {
    pub number: u32,
    pub title: String,
    pub body: String,
    pub source: IssueOrigin,
    pub target_repo: Option<String>,
}

/// Where an issue originated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueOrigin {
    Github,
    Local,
}

impl IssueOrigin {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Github => "github",
            Self::Local => "local",
        }
    }
}

impl std::fmt::Display for IssueOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Trait for fetching and transitioning issues regardless of source.
#[async_trait]
pub trait IssueProvider: Send + Sync {
    /// Fetch all open issues with the given label.
    async fn get_ready_issues(&self, label: &str) -> Result<Vec<PipelineIssue>>;

    /// Fetch a single issue by number.
    async fn get_issue(&self, number: u32) -> Result<PipelineIssue>;

    /// Transition an issue from one label to another.
    async fn transition(&self, number: u32, from: &str, to: &str) -> Result<()>;

    /// Post a comment on an issue (or append to the local issue body).
    async fn comment(&self, number: u32, body: &str) -> Result<()>;

    /// Close an issue.
    async fn close(&self, number: u32, comment: Option<&str>) -> Result<()>;
}
