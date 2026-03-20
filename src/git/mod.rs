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

/// Create an empty commit (used to seed a branch before PR creation).
pub async fn empty_commit(repo_dir: &Path, message: &str) -> Result<()> {
    run_git(repo_dir, &["commit", "--allow-empty", "-m", message])
        .await
        .context("creating empty commit")?;
    Ok(())
}

/// Push a branch to origin.
pub async fn push_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    run_git(repo_dir, &["push", "origin", branch]).await.context("pushing branch")?;
    Ok(())
}

/// Force-push a branch to origin using `--force-with-lease` for safety.
///
/// Used after rebasing a pipeline branch onto the updated base branch.
pub async fn force_push_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    let lease = format!("--force-with-lease=refs/heads/{branch}");
    run_git(repo_dir, &["push", &lease, "origin", branch]).await.context("force-pushing branch")?;
    Ok(())
}

/// Outcome of a rebase attempt.
#[derive(Debug)]
pub enum RebaseOutcome {
    /// Rebase succeeded cleanly.
    Clean,
    /// Rebase had conflicts. The working tree is left in a mid-rebase state
    /// so the caller can attempt agent-assisted resolution via
    /// [`rebase_continue`] / [`abort_rebase`].
    RebaseConflicts(Vec<String>),
    /// Agent resolved the rebase conflicts.
    AgentResolved,
    /// Unrecoverable failure (e.g. fetch failed).
    Failed(String),
}

/// Start a rebase of the current branch onto the latest `origin/<base_branch>`.
///
/// If the rebase succeeds cleanly, returns `Clean`. If it hits conflicts, the
/// working tree is left in a mid-rebase state and `RebaseConflicts` is returned
/// with the list of conflicting files. The caller should resolve them and call
/// [`rebase_continue`], or [`abort_rebase`] to give up.
pub async fn start_rebase(repo_dir: &Path, base_branch: &str) -> RebaseOutcome {
    if let Err(e) = run_git(repo_dir, &["fetch", "origin", base_branch]).await {
        return RebaseOutcome::Failed(format!("failed to fetch {base_branch}: {e}"));
    }

    let target = format!("origin/{base_branch}");

    let no_editor = [("GIT_EDITOR", "true")];
    if run_git_with_env(repo_dir, &["rebase", &target], &no_editor).await.is_ok() {
        return RebaseOutcome::Clean;
    }

    // Rebase stopped -- check if it's real conflicts or an empty commit.
    let conflicting = conflicting_files(repo_dir).await;
    if conflicting.is_empty() {
        // No conflicting files means an empty commit (patch already applied).
        // Skip it rather than sending an agent to resolve nothing.
        match skip_empty_rebase_commits(repo_dir).await {
            Ok(None) => return RebaseOutcome::Clean,
            Ok(Some(files)) => return RebaseOutcome::RebaseConflicts(files),
            Err(e) => return RebaseOutcome::Failed(format!("{e:#}")),
        }
    }
    RebaseOutcome::RebaseConflicts(conflicting)
}

/// List files with unresolved merge conflicts.
pub async fn conflicting_files(repo_dir: &Path) -> Vec<String> {
    run_git(repo_dir, &["diff", "--name-only", "--diff-filter=U"])
        .await
        .map_or_else(|_| vec![], |output| output.lines().map(String::from).collect())
}

/// Abort an in-progress rebase.
pub async fn abort_rebase(repo_dir: &Path) {
    let _ = run_git(repo_dir, &["rebase", "--abort"]).await;
}

/// Skip empty commits during a rebase when no conflicting files are present.
///
/// During rebase replay, a commit can become empty if its changes are already
/// present on the target branch. Git stops on these by default. This function
/// runs `git rebase --skip` in a loop until the rebase completes or real
/// conflicts appear.
///
/// Returns `Ok(None)` if the rebase completed after skipping.
/// Returns `Ok(Some(files))` if real conflicts appeared after a skip.
/// Returns `Err` if the maximum number of skips was exhausted.
async fn skip_empty_rebase_commits(repo_dir: &Path) -> Result<Option<Vec<String>>> {
    const MAX_SKIPS: u32 = 10;
    let no_editor = [("GIT_EDITOR", "true")];

    for _ in 0..MAX_SKIPS {
        if run_git_with_env(repo_dir, &["rebase", "--skip"], &no_editor).await.is_ok() {
            return Ok(None);
        }

        // Skip stopped again -- check for real conflicts vs another empty commit.
        let conflicts = conflicting_files(repo_dir).await;
        if !conflicts.is_empty() {
            return Ok(Some(conflicts));
        }
    }

    abort_rebase(repo_dir).await;
    anyhow::bail!("rebase had too many empty commits (skipped {MAX_SKIPS} times)")
}

/// Stage resolved conflict files and continue the in-progress rebase.
///
/// Returns `Ok(None)` if the rebase completed successfully after continuing.
/// Returns `Ok(Some(files))` if continuing hit new conflicts on the next commit.
/// Returns `Err` on unexpected failures.
pub async fn rebase_continue(
    repo_dir: &Path,
    conflicting: &[String],
) -> Result<Option<Vec<String>>> {
    for file in conflicting {
        run_git(repo_dir, &["add", "--", file]).await.with_context(|| format!("staging {file}"))?;
    }

    let no_editor = [("GIT_EDITOR", "true")];
    if run_git_with_env(repo_dir, &["rebase", "--continue"], &no_editor).await.is_ok() {
        return Ok(None);
    }

    // rebase --continue stopped again -- new conflicts on the next commit.
    let new_conflicts = conflicting_files(repo_dir).await;
    if new_conflicts.is_empty() {
        // Empty commit after continue -- skip it.
        return skip_empty_rebase_commits(repo_dir).await;
    }
    Ok(Some(new_conflicts))
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

/// Get the current HEAD commit SHA.
pub async fn head_sha(repo_dir: &Path) -> Result<String> {
    run_git(repo_dir, &["rev-parse", "HEAD"]).await.context("getting HEAD sha")
}

/// Count commits between a ref and HEAD.
pub async fn commit_count_since(repo_dir: &Path, since_ref: &str) -> Result<u32> {
    let output = run_git(repo_dir, &["rev-list", "--count", &format!("{since_ref}..HEAD")])
        .await
        .context("counting commits since ref")?;
    output.parse::<u32>().context("parsing commit count")
}

/// List files changed between a ref and HEAD.
pub async fn changed_files_since(repo_dir: &Path, since_ref: &str) -> Result<Vec<String>> {
    let output = run_git(repo_dir, &["diff", "--name-only", since_ref, "HEAD"])
        .await
        .context("listing changed files since ref")?;
    Ok(output.lines().filter(|l| !l.is_empty()).map(String::from).collect())
}

async fn run_git(repo_dir: &Path, args: &[&str]) -> Result<String> {
    run_git_with_env(repo_dir, args, &[]).await
}

async fn run_git_with_env(repo_dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo_dir).kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().await.context("spawning git")?;

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

    #[tokio::test]
    async fn force_push_branch_works() {
        let dir = init_temp_repo().await;

        // Set up a bare remote to push to
        let remote_dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", "--bare", &dir.path().to_string_lossy(), "."])
            .current_dir(remote_dir.path())
            .output()
            .await
            .unwrap();

        run_git(dir.path(), &["remote", "add", "origin", &remote_dir.path().to_string_lossy()])
            .await
            .unwrap();

        // Create a branch, push it, then amend and force-push
        run_git(dir.path(), &["checkout", "-b", "test-branch"]).await.unwrap();
        tokio::fs::write(dir.path().join("new.txt"), "v1").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "v1"]).await.unwrap();
        push_branch(dir.path(), "test-branch").await.unwrap();

        // Amend the commit (simulating a rebase)
        tokio::fs::write(dir.path().join("new.txt"), "v2").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "--amend", "-m", "v2"]).await.unwrap();

        // Regular push should fail, force push should succeed
        assert!(push_branch(dir.path(), "test-branch").await.is_err());
        assert!(force_push_branch(dir.path(), "test-branch").await.is_ok());
    }

    #[tokio::test]
    async fn start_rebase_clean() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        run_git(dir.path(), &["checkout", "-b", "feature"]).await.unwrap();
        tokio::fs::write(dir.path().join("feature.txt"), "feature work").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature commit"]).await.unwrap();

        run_git(dir.path(), &["checkout", &branch]).await.unwrap();
        tokio::fs::write(dir.path().join("base.txt"), "base work").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "base commit"]).await.unwrap();

        run_git(dir.path(), &["checkout", "feature"]).await.unwrap();
        run_git(dir.path(), &["remote", "add", "origin", &dir.path().to_string_lossy()])
            .await
            .unwrap();

        let outcome = start_rebase(dir.path(), &branch).await;
        assert!(matches!(outcome, RebaseOutcome::Clean));
        assert!(dir.path().join("feature.txt").exists());
        assert!(dir.path().join("base.txt").exists());
    }

    #[tokio::test]
    async fn start_rebase_conflicts() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        // Create a feature branch that modifies README.md
        run_git(dir.path(), &["checkout", "-b", "feature"]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "feature version").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature change"]).await.unwrap();

        // Create a conflicting change on the base branch
        run_git(dir.path(), &["checkout", &branch]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "base version").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "base change"]).await.unwrap();

        run_git(dir.path(), &["checkout", "feature"]).await.unwrap();
        run_git(dir.path(), &["remote", "add", "origin", &dir.path().to_string_lossy()])
            .await
            .unwrap();

        let outcome = start_rebase(dir.path(), &branch).await;
        assert!(
            matches!(outcome, RebaseOutcome::RebaseConflicts(_)),
            "expected RebaseConflicts, got {outcome:?}"
        );

        // Tree should be in mid-rebase state
        assert!(
            dir.path().join(".git/rebase-merge").exists()
                || dir.path().join(".git/rebase-apply").exists()
        );

        // Clean up
        abort_rebase(dir.path()).await;
    }

    #[tokio::test]
    async fn start_rebase_no_remote_fails() {
        let dir = init_temp_repo().await;
        // No remote configured, so fetch will fail
        let outcome = start_rebase(dir.path(), "main").await;
        assert!(matches!(outcome, RebaseOutcome::Failed(_)));
    }

    #[tokio::test]
    async fn rebase_continue_resolves_conflict() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        run_git(dir.path(), &["checkout", "-b", "feature"]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "feature version").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature change"]).await.unwrap();

        run_git(dir.path(), &["checkout", &branch]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "base version").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "base change"]).await.unwrap();

        run_git(dir.path(), &["checkout", "feature"]).await.unwrap();
        run_git(dir.path(), &["remote", "add", "origin", &dir.path().to_string_lossy()])
            .await
            .unwrap();

        let outcome = start_rebase(dir.path(), &branch).await;
        let files = match outcome {
            RebaseOutcome::RebaseConflicts(f) => f,
            other => panic!("expected RebaseConflicts, got {other:?}"),
        };

        // Manually resolve the conflict
        tokio::fs::write(dir.path().join("README.md"), "resolved version").await.unwrap();

        let result = rebase_continue(dir.path(), &files).await.unwrap();
        assert!(result.is_none(), "expected rebase to complete, got more conflicts");

        // Verify rebase completed (no rebase-merge dir)
        assert!(!dir.path().join(".git/rebase-merge").exists());
        assert!(!dir.path().join(".git/rebase-apply").exists());
    }

    #[tokio::test]
    async fn start_rebase_skips_empty_commit() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        // Create a feature branch with a change
        run_git(dir.path(), &["checkout", "-b", "feature"]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "changed").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature change"]).await.unwrap();

        // Cherry-pick the same change onto base so the feature commit becomes empty
        run_git(dir.path(), &["checkout", &branch]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "changed").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "same change on base"]).await.unwrap();

        run_git(dir.path(), &["checkout", "feature"]).await.unwrap();
        run_git(dir.path(), &["remote", "add", "origin", &dir.path().to_string_lossy()])
            .await
            .unwrap();

        // Rebase should skip the empty commit and succeed
        let outcome = start_rebase(dir.path(), &branch).await;
        assert!(
            matches!(outcome, RebaseOutcome::Clean),
            "expected Clean after skipping empty commit, got {outcome:?}"
        );

        // Verify rebase completed
        assert!(!dir.path().join(".git/rebase-merge").exists());
        assert!(!dir.path().join(".git/rebase-apply").exists());
    }

    #[tokio::test]
    async fn rebase_continue_skips_empty_commit_after_real_conflict() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        // Feature branch: commit 1 changes README, commit 2 changes other.txt
        run_git(dir.path(), &["checkout", "-b", "feature"]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "feature readme").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature readme"]).await.unwrap();

        tokio::fs::write(dir.path().join("other.txt"), "feature other").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature other"]).await.unwrap();

        // Base branch: conflict on README, cherry-pick the same other.txt change
        run_git(dir.path(), &["checkout", &branch]).await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "base readme").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "base readme"]).await.unwrap();

        tokio::fs::write(dir.path().join("other.txt"), "feature other").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "same other on base"]).await.unwrap();

        run_git(dir.path(), &["checkout", "feature"]).await.unwrap();
        run_git(dir.path(), &["remote", "add", "origin", &dir.path().to_string_lossy()])
            .await
            .unwrap();

        // First commit (README) will have a real conflict
        let outcome = start_rebase(dir.path(), &branch).await;
        let files = match outcome {
            RebaseOutcome::RebaseConflicts(f) => f,
            other => panic!("expected RebaseConflicts, got {other:?}"),
        };
        assert!(files.contains(&"README.md".to_string()));

        // Resolve the conflict manually
        tokio::fs::write(dir.path().join("README.md"), "resolved").await.unwrap();

        // Continue should resolve the conflict, then skip the empty second commit
        let result = rebase_continue(dir.path(), &files).await.unwrap();
        assert!(result.is_none(), "expected rebase to complete, got more conflicts");

        assert!(!dir.path().join(".git/rebase-merge").exists());
        assert!(!dir.path().join(".git/rebase-apply").exists());
    }
}
