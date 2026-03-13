use std::{fmt::Write, path::PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{IssueOrigin, IssueProvider, PipelineIssue};

/// Reads issues from `.oven/issues/*.md` files.
pub struct LocalIssueProvider {
    issues_dir: PathBuf,
}

impl LocalIssueProvider {
    pub fn new(project_dir: &std::path::Path) -> Self {
        Self { issues_dir: project_dir.join(".oven").join("issues") }
    }
}

/// Parsed ticket frontmatter from a local issue file.
#[derive(Debug)]
struct LocalTicket {
    id: u32,
    title: String,
    status: String,
    labels: Vec<String>,
    target_repo: Option<String>,
    body: String,
}

/// Parse a local issue markdown file into its components.
fn parse_local_issue(content: &str) -> Result<LocalTicket> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        anyhow::bail!("missing frontmatter delimiters");
    }

    let after_open = &content[3..];
    let close_idx = after_open.find("\n---").context("missing closing frontmatter delimiter")?;

    let frontmatter = &after_open[..close_idx];
    let body = after_open[close_idx + 4..].trim_start_matches('\n').to_string();

    let mut id = 0u32;
    let mut title = String::new();
    let mut status = "open".to_string();
    let mut labels = Vec::new();
    let mut target_repo = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("id:") {
            id = val.trim().parse().context("invalid id")?;
        } else if let Some(val) = line.strip_prefix("title:") {
            title = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("status:") {
            status = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("labels:") {
            // Parse ["label1", "label2"] format
            let val = val.trim();
            if val.starts_with('[') && val.ends_with(']') {
                let inner = &val[1..val.len() - 1];
                labels = inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        } else if let Some(val) = line.strip_prefix("target_repo:") {
            target_repo = Some(val.trim().to_string());
        }
    }

    Ok(LocalTicket { id, title, status, labels, target_repo, body })
}

/// Rewrite the labels line in a frontmatter block.
fn rewrite_frontmatter_labels(content: &str, labels: &[String]) -> String {
    let labels_str = labels.iter().map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(", ");
    let new_labels_line = format!("labels: [{labels_str}]");

    let mut result = String::new();
    for line in content.lines() {
        if line.trim().starts_with("labels:") {
            result.push_str(&new_labels_line);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

#[async_trait]
impl IssueProvider for LocalIssueProvider {
    async fn get_ready_issues(&self, label: &str) -> Result<Vec<PipelineIssue>> {
        if !self.issues_dir.exists() {
            return Ok(Vec::new());
        }

        let mut issues = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(&self.issues_dir)
            .context("reading issues directory")?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .collect();

        // Sort by filename for FIFO ordering
        entries.sort_by_key(std::fs::DirEntry::path);

        for entry in entries {
            let content = std::fs::read_to_string(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if let Ok(ticket) = parse_local_issue(&content) {
                if ticket.status == "open" && ticket.labels.iter().any(|l| l == label) {
                    issues.push(PipelineIssue {
                        number: ticket.id,
                        title: ticket.title,
                        body: ticket.body,
                        source: IssueOrigin::Local,
                        target_repo: ticket.target_repo,
                    });
                }
            }
        }

        Ok(issues)
    }

    async fn get_issue(&self, number: u32) -> Result<PipelineIssue> {
        let path = self.issues_dir.join(format!("{number}.md"));
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("ticket #{number} not found"))?;
        let ticket = parse_local_issue(&content)?;
        Ok(PipelineIssue {
            number: ticket.id,
            title: ticket.title,
            body: ticket.body,
            source: IssueOrigin::Local,
            target_repo: ticket.target_repo,
        })
    }

    async fn transition(&self, number: u32, from: &str, to: &str) -> Result<()> {
        let path = self.issues_dir.join(format!("{number}.md"));
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("ticket #{number} not found"))?;
        let mut ticket = parse_local_issue(&content)?;
        ticket.labels.retain(|l| l != from);
        if !ticket.labels.contains(&to.to_string()) {
            ticket.labels.push(to.to_string());
        }
        let updated = rewrite_frontmatter_labels(&content, &ticket.labels);
        std::fs::write(&path, updated).context("writing updated ticket")?;
        Ok(())
    }

    async fn comment(&self, number: u32, body: &str) -> Result<()> {
        let path = self.issues_dir.join(format!("{number}.md"));
        let mut content = std::fs::read_to_string(&path)
            .with_context(|| format!("ticket #{number} not found"))?;
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
        let _ = write!(content, "\n---\n\n**Comment ({now}):**\n\n{body}\n");
        std::fs::write(&path, content).context("writing comment")?;
        Ok(())
    }

    async fn close(&self, number: u32, comment: Option<&str>) -> Result<()> {
        let path = self.issues_dir.join(format!("{number}.md"));
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("ticket #{number} not found"))?;
        let updated = content.replace("status: open", "status: closed");
        std::fs::write(&path, &updated).context("writing closed ticket")?;

        if let Some(body) = comment {
            self.comment(number, body).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_issue_file(dir: &std::path::Path, id: u32, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{id}.md")), content).unwrap();
    }

    fn issue_content(id: u32, title: &str, status: &str, labels: &[&str]) -> String {
        let labels_str = labels.iter().map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(", ");
        format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\nlabels: [{labels_str}]\n---\n\nIssue body for {title}"
        )
    }

    #[tokio::test]
    async fn get_ready_issues_returns_matching() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 1, &issue_content(1, "First", "open", &["o-ready"]));
        create_issue_file(&issues_dir, 2, &issue_content(2, "Second", "open", &["o-cooking"]));
        create_issue_file(&issues_dir, 3, &issue_content(3, "Third", "open", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        let issues = provider.get_ready_issues("o-ready").await.unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[1].number, 3);
        assert_eq!(issues[0].source, IssueOrigin::Local);
    }

    #[tokio::test]
    async fn get_ready_issues_skips_closed() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 1, &issue_content(1, "Open", "open", &["o-ready"]));
        create_issue_file(&issues_dir, 2, &issue_content(2, "Closed", "closed", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        let issues = provider.get_ready_issues("o-ready").await.unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 1);
    }

    #[tokio::test]
    async fn get_ready_issues_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalIssueProvider::new(dir.path());
        let issues = provider.get_ready_issues("o-ready").await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn get_issue_returns_specific() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 42, &issue_content(42, "Specific", "open", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        let issue = provider.get_issue(42).await.unwrap();

        assert_eq!(issue.number, 42);
        assert_eq!(issue.title, "Specific");
    }

    #[tokio::test]
    async fn get_issue_nonexistent_errors() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalIssueProvider::new(dir.path());
        let result = provider.get_issue(999).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn transition_updates_labels() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 1, &issue_content(1, "Test", "open", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        provider.transition(1, "o-ready", "o-cooking").await.unwrap();

        let issue = provider.get_issue(1).await.unwrap();
        // Verify by re-reading: the issue should no longer match o-ready
        let issues = provider.get_ready_issues("o-ready").await.unwrap();
        assert!(issues.is_empty());

        let cooking = provider.get_ready_issues("o-cooking").await.unwrap();
        assert_eq!(cooking.len(), 1);
        assert_eq!(cooking[0].number, issue.number);
    }

    #[tokio::test]
    async fn comment_appends_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 1, &issue_content(1, "Test", "open", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        provider.comment(1, "Pipeline started").await.unwrap();

        let content = std::fs::read_to_string(issues_dir.join("1.md")).unwrap();
        assert!(content.contains("Pipeline started"));
        assert!(content.contains("Comment"));
    }

    #[tokio::test]
    async fn close_sets_status() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 1, &issue_content(1, "Test", "open", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        provider.close(1, Some("Done")).await.unwrap();

        let content = std::fs::read_to_string(issues_dir.join("1.md")).unwrap();
        assert!(content.contains("status: closed"));
        assert!(content.contains("Done"));
    }

    #[tokio::test]
    async fn target_repo_parsed_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        let content = "---\nid: 5\ntitle: Multi-repo\nstatus: open\nlabels: [\"o-ready\"]\ntarget_repo: api\n---\n\nDo work";
        create_issue_file(&issues_dir, 5, content);

        let provider = LocalIssueProvider::new(dir.path());
        let issue = provider.get_issue(5).await.unwrap();

        assert_eq!(issue.target_repo.as_deref(), Some("api"));
    }

    #[tokio::test]
    async fn target_repo_none_when_not_in_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let issues_dir = dir.path().join(".oven").join("issues");

        create_issue_file(&issues_dir, 1, &issue_content(1, "Normal", "open", &["o-ready"]));

        let provider = LocalIssueProvider::new(dir.path());
        let issue = provider.get_issue(1).await.unwrap();

        assert!(issue.target_repo.is_none());
    }

    #[test]
    fn parse_local_issue_valid() {
        let content =
            "---\nid: 1\ntitle: Test\nstatus: open\nlabels: [\"o-ready\"]\n---\n\nBody text";
        let ticket = parse_local_issue(content).unwrap();
        assert_eq!(ticket.id, 1);
        assert_eq!(ticket.title, "Test");
        assert_eq!(ticket.status, "open");
        assert_eq!(ticket.labels, vec!["o-ready"]);
        assert_eq!(ticket.body, "Body text");
    }

    #[test]
    fn parse_local_issue_missing_frontmatter() {
        let result = parse_local_issue("No frontmatter here");
        assert!(result.is_err());
    }

    #[test]
    fn rewrite_labels_preserves_rest() {
        let content = "---\nid: 1\ntitle: Test\nstatus: open\nlabels: [\"o-ready\"]\n---\n\nBody";
        let result = rewrite_frontmatter_labels(content, &["o-cooking".to_string()]);
        assert!(result.contains("labels: [\"o-cooking\"]"));
        assert!(result.contains("id: 1"));
        assert!(result.contains("title: Test"));
    }
}
