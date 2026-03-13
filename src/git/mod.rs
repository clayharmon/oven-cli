use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

/// A git worktree created for an issue pipeline.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub issue_number: u32,
}

/// Info about an existing worktree from `git worktree list`.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Generate a branch name for an issue: `oven/issue-{number}-{short_hex}`.
fn branch_name(issue_number: u32) -> String {
    let short_hex = &uuid::Uuid::new_v4().to_string()[..8];
    format!("oven/issue-{issue_number}-{short_hex}")
}

/// Create a worktree for the given issue, branching from `base_branch`.
pub async fn create_worktree(
    repo_dir: &Path,
    issue_number: u32,
    base_branch: &str,
) -> Result<Worktree> {
    let branch = branch_name(issue_number);
    let worktree_path =
        repo_dir.join(".oven").join("worktrees").join(format!("issue-{issue_number}"));

    // Ensure parent directory exists
    if let Some(parent) = worktree_path.parent() {
        tokio::fs::create_dir_all(parent).await.context("creating worktree parent directory")?;
    }

    run_git(
        repo_dir,
        &["worktree", "add", "-b", &branch, &worktree_path.to_string_lossy(), base_branch],
    )
    .await
    .context("creating worktree")?;

    Ok(Worktree { path: worktree_path, branch, issue_number })
}

/// Remove a worktree by path.
pub async fn remove_worktree(repo_dir: &Path, worktree_path: &Path) -> Result<()> {
    run_git(repo_dir, &["worktree", "remove", "--force", &worktree_path.to_string_lossy()])
        .await
        .context("removing worktree")?;
    Ok(())
}

/// List all worktrees in the repository.
pub async fn list_worktrees(repo_dir: &Path) -> Result<Vec<WorktreeInfo>> {
    let output = run_git(repo_dir, &["worktree", "list", "--porcelain"])
        .await
        .context("listing worktrees")?;

    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            // Save previous worktree if we have one
            if let Some(path) = current_path.take() {
                worktrees.push(WorktreeInfo { path, branch: current_branch.take() });
            }
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // Extract branch name from refs/heads/...
            current_branch =
                Some(branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref).to_string());
        }
    }

    // Don't forget the last one
    if let Some(path) = current_path {
        worktrees.push(WorktreeInfo { path, branch: current_branch });
    }

    Ok(worktrees)
}

/// Prune stale worktrees and return the count pruned.
pub async fn clean_worktrees(repo_dir: &Path) -> Result<u32> {
    let before = list_worktrees(repo_dir).await?;
    run_git(repo_dir, &["worktree", "prune"]).await.context("pruning worktrees")?;
    let after = list_worktrees(repo_dir).await?;

    let pruned = if before.len() > after.len() { before.len() - after.len() } else { 0 };
    Ok(u32::try_from(pruned).unwrap_or(u32::MAX))
}

/// Delete a local branch.
pub async fn delete_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    run_git(repo_dir, &["branch", "-D", branch]).await.context("deleting branch")?;
    Ok(())
}

/// List merged branches matching `oven/*`.
pub async fn list_merged_branches(repo_dir: &Path, base: &str) -> Result<Vec<String>> {
    let output = run_git(repo_dir, &["branch", "--merged", base])
        .await
        .context("listing merged branches")?;

    let branches = output
        .lines()
        .map(|l| l.trim().trim_start_matches("* ").to_string())
        .filter(|b| b.starts_with("oven/"))
        .collect();

    Ok(branches)
}

/// Push a branch to origin.
pub async fn push_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    run_git(repo_dir, &["push", "origin", branch]).await.context("pushing branch")?;
    Ok(())
}

/// Get the default branch name (main or master).
pub async fn default_branch(repo_dir: &Path) -> Result<String> {
    // Try symbolic-ref first
    if let Ok(output) = run_git(repo_dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await {
        if let Some(branch) = output.strip_prefix("refs/remotes/origin/") {
            return Ok(branch.to_string());
        }
    }

    // Fallback: check if main exists, otherwise master
    if run_git(repo_dir, &["rev-parse", "--verify", "main"]).await.is_ok() {
        return Ok("main".to_string());
    }
    if run_git(repo_dir, &["rev-parse", "--verify", "master"]).await.is_ok() {
        return Ok("master".to_string());
    }

    // Last resort: whatever HEAD points to
    let output = run_git(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .context("detecting default branch")?;
    Ok(output)
}

async fn run_git(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        // Init a repo with an initial commit so we have a branch to work from
        Command::new("git").args(["init"]).current_dir(dir.path()).output().await.unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        tokio::fs::write(dir.path().join("README.md"), "hello").await.unwrap();

        Command::new("git").args(["add", "."]).current_dir(dir.path()).output().await.unwrap();

        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        dir
    }

    #[tokio::test]
    async fn create_and_remove_worktree() {
        let dir = init_temp_repo().await;

        // Detect the current branch name
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        let wt = create_worktree(dir.path(), 42, &branch).await.unwrap();
        assert!(wt.path.exists());
        assert!(wt.branch.starts_with("oven/issue-42-"));
        assert_eq!(wt.issue_number, 42);

        remove_worktree(dir.path(), &wt.path).await.unwrap();
        assert!(!wt.path.exists());
    }

    #[tokio::test]
    async fn list_worktrees_includes_created() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        let _wt = create_worktree(dir.path(), 99, &branch).await.unwrap();

        let worktrees = list_worktrees(dir.path()).await.unwrap();
        // Should have at least the main worktree + the one we created
        assert!(worktrees.len() >= 2);
        assert!(
            worktrees
                .iter()
                .any(|w| { w.branch.as_deref().is_some_and(|b| b.starts_with("oven/issue-99-")) })
        );
    }

    #[tokio::test]
    async fn branch_naming_convention() {
        let name = branch_name(123);
        assert!(name.starts_with("oven/issue-123-"));
        assert_eq!(name.len(), "oven/issue-123-".len() + 8);
        // The hex part should be valid hex
        let hex_part = &name["oven/issue-123-".len()..];
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn default_branch_detection() {
        let dir = init_temp_repo().await;
        let branch = default_branch(dir.path()).await.unwrap();
        // git init creates "main" or "master" depending on config
        assert!(branch == "main" || branch == "master", "got: {branch}");
    }

    #[tokio::test]
    async fn error_on_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = list_worktrees(dir.path()).await;
        assert!(result.is_err());
    }
}
