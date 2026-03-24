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

/// Create a worktree for the given issue, branching from `origin/<base_branch>`.
///
/// Uses the remote tracking ref rather than the local branch so that worktrees
/// always start from the latest remote state (e.g. after PRs are merged).
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

    let start_point = format!("origin/{base_branch}");
    run_git(
        repo_dir,
        &["worktree", "add", "-b", &branch, &worktree_path.to_string_lossy(), &start_point],
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

/// Check if the worktree has uncommitted changes (staged or unstaged tracked files).
pub async fn is_dirty(repo_dir: &Path) -> Result<bool> {
    let output =
        run_git(repo_dir, &["status", "--porcelain"]).await.context("checking dirty state")?;
    Ok(!output.is_empty())
}

/// Stage modified/deleted tracked files and commit them with the given message.
///
/// Used to preserve agent work (e.g. fixer edits) that wasn't committed before
/// the rebase step. Uses `git add -u` (tracked files only) rather than `git add -A`
/// to avoid committing untracked artifacts like temp files or test output.
///
/// Checks for tracked changes with `-uno` (ignore untracked) so that worktrees
/// with only untracked files don't trigger a commit attempt that would fail.
///
/// Returns `Ok(true)` if a commit was created, `Ok(false)` if there was nothing
/// to commit.
pub async fn commit_all(repo_dir: &Path, message: &str) -> Result<bool> {
    let tracked_changes = run_git(repo_dir, &["status", "--porcelain", "-uno"])
        .await
        .context("checking tracked changes")?;
    if tracked_changes.is_empty() {
        return Ok(false);
    }
    run_git(repo_dir, &["add", "-u"]).await.context("staging tracked changes")?;
    run_git(repo_dir, &["commit", "-m", message]).await.context("committing changes")?;
    Ok(true)
}

/// Push a branch to origin.
pub async fn push_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    run_git(repo_dir, &["push", "origin", branch]).await.context("pushing branch")?;
    Ok(())
}

/// Fetch a branch from origin to update the remote tracking ref.
///
/// Used between pipeline layers so that new worktrees (which branch from
/// `origin/<branch>`) start from post-merge state. Retries once on transient
/// ref lock contention from parallel fetches.
pub async fn fetch_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    fetch_with_retry(repo_dir, branch)
        .await
        .with_context(|| format!("fetching {branch} from origin"))
}

/// Advance the local branch ref to match `origin/<branch>` after a fetch.
///
/// If the branch is currently checked out, uses `merge --ff-only` so the
/// working tree stays in sync. Otherwise updates the ref directly after
/// verifying the move is a fast-forward. Errors are non-fatal for the
/// pipeline (which only needs `origin/<branch>`), but keeping the local
/// branch current avoids surprise "behind by N commits" messages.
pub async fn advance_local_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    let remote_ref = format!("origin/{branch}");
    let current = run_git(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .context("detecting current branch")?;

    if current == branch {
        run_git(repo_dir, &["merge", "--ff-only", &remote_ref])
            .await
            .context("fast-forwarding checked-out branch")?;
    } else {
        // Only update if it's a fast-forward (local is ancestor of remote).
        if run_git(repo_dir, &["merge-base", "--is-ancestor", branch, &remote_ref]).await.is_ok() {
            run_git(repo_dir, &["branch", "-f", branch, &remote_ref])
                .await
                .context("updating local branch ref")?;
        }
    }
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
///
/// Uses `--empty=drop` so commits that become empty after rebase (because their
/// changes are already on the target branch) are silently dropped instead of
/// stopping the rebase.
pub async fn start_rebase(repo_dir: &Path, base_branch: &str) -> RebaseOutcome {
    if let Err(e) = fetch_with_retry(repo_dir, base_branch).await {
        return RebaseOutcome::Failed(format!("failed to fetch {base_branch}: {e}"));
    }

    let target = format!("origin/{base_branch}");

    let no_editor = [("GIT_EDITOR", "true")];
    let Err(rebase_err) =
        run_git_with_env(repo_dir, &["rebase", "--empty=drop", &target], &no_editor).await
    else {
        return RebaseOutcome::Clean;
    };

    // Rebase command failed. Check if a rebase is actually in progress -- if not,
    // the failure was something else entirely (dirty worktree, invalid ref, etc.)
    // and we should report the real error rather than misclassifying it.
    if !rebase_in_progress(repo_dir).await {
        return RebaseOutcome::Failed(format!("rebase could not start: {rebase_err}"));
    }

    let conflicting = conflicting_files(repo_dir).await;
    if conflicting.is_empty() {
        // Rebase is in progress but no conflicting files. This shouldn't happen
        // with --empty=drop (empty commits are auto-dropped), but handle it
        // gracefully by aborting and reporting the unexpected state.
        abort_rebase(repo_dir).await;
        return RebaseOutcome::Failed(
            "rebase stopped with no conflicts and no empty commits".to_string(),
        );
    }
    RebaseOutcome::RebaseConflicts(conflicting)
}

/// List files with unresolved merge conflicts (git index state).
///
/// Uses `git diff --diff-filter=U` which checks the index, not file content.
/// A file stays "Unmerged" until `git add` is run on it, even if conflict
/// markers have been removed from the working tree.
pub async fn conflicting_files(repo_dir: &Path) -> Vec<String> {
    run_git(repo_dir, &["diff", "--name-only", "--diff-filter=U"])
        .await
        .map_or_else(|_| vec![], |output| output.lines().map(String::from).collect())
}

/// Check which files still contain conflict markers in their content.
///
/// Unlike [`conflicting_files`], this reads the actual working tree content
/// rather than relying on git index state. This is the correct check after an
/// agent edits files to resolve conflicts but before `git add` is run.
pub async fn files_with_conflict_markers(repo_dir: &Path, files: &[String]) -> Vec<String> {
    let mut unresolved = Vec::new();
    for file in files {
        let path = repo_dir.join(file);
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if content.contains("<<<<<<<") || content.contains(">>>>>>>") {
                unresolved.push(file.clone());
            }
        }
    }
    unresolved
}

/// Abort an in-progress rebase.
pub async fn abort_rebase(repo_dir: &Path) {
    let _ = run_git(repo_dir, &["rebase", "--abort"]).await;
}

/// Check whether a rebase is currently in progress.
///
/// Git creates `.git/rebase-merge` (for interactive/standard rebase) or
/// `.git/rebase-apply` (for `git am` / `git rebase --apply`) while a rebase
/// is active. Worktrees store these under `.git/worktrees/<name>/` instead,
/// so we use `git rev-parse --git-dir` to find the correct location.
pub async fn rebase_in_progress(repo_dir: &Path) -> bool {
    let git_dir = run_git(repo_dir, &["rev-parse", "--git-dir"])
        .await
        .map_or_else(|_| repo_dir.join(".git"), |s| PathBuf::from(s.trim()));

    let git_dir = if git_dir.is_absolute() { git_dir } else { repo_dir.join(git_dir) };

    tokio::fs::try_exists(git_dir.join("rebase-merge")).await.unwrap_or(false)
        || tokio::fs::try_exists(git_dir.join("rebase-apply")).await.unwrap_or(false)
}

/// Fetch with a single retry for transient failures (e.g. ref lock contention).
async fn fetch_with_retry(repo_dir: &Path, branch: &str) -> Result<()> {
    match run_git(repo_dir, &["fetch", "origin", branch]).await {
        Ok(_) => Ok(()),
        Err(first_err) => {
            let msg = format!("{first_err:#}");
            if msg.contains("unable to update local ref") || msg.contains("cannot lock ref") {
                tracing::warn!(branch, "fetch failed with ref lock contention, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                run_git(repo_dir, &["fetch", "origin", branch])
                    .await
                    .map(|_| ())
                    .with_context(|| format!("retry fetch {branch}"))
            } else {
                Err(first_err)
            }
        }
    }
}

/// Stage resolved conflict files and continue the in-progress rebase.
///
/// Returns `Ok(None)` if the rebase completed successfully after continuing.
/// Returns `Ok(Some(files))` if continuing hit new conflicts on the next commit.
/// Returns `Err` on unexpected failures.
///
/// Because the parent `start_rebase` uses `--empty=drop`, any commits that
/// become empty after conflict resolution are automatically dropped by git
/// during `--continue`.
pub async fn rebase_continue(
    repo_dir: &Path,
    conflicting: &[String],
) -> Result<Option<Vec<String>>> {
    for file in conflicting {
        run_git(repo_dir, &["add", "--", file]).await.with_context(|| format!("staging {file}"))?;
    }

    let no_editor = [("GIT_EDITOR", "true")];
    let Err(continue_err) = run_git_with_env(repo_dir, &["rebase", "--continue"], &no_editor).await
    else {
        return Ok(None);
    };

    // rebase --continue stopped again -- check for new conflicts on the next commit.
    if !rebase_in_progress(repo_dir).await {
        anyhow::bail!("rebase --continue failed and no rebase is in progress: {continue_err}");
    }

    let new_conflicts = conflicting_files(repo_dir).await;
    if new_conflicts.is_empty() {
        anyhow::bail!("rebase stopped after continue with no conflicts: {continue_err}");
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

    /// Create a temp repo with a bare remote so `origin/<branch>` exists.
    /// Returns (repo dir, remote dir) -- both must be kept alive for the test.
    async fn init_temp_repo_with_remote() -> (tempfile::TempDir, tempfile::TempDir) {
        let dir = init_temp_repo().await;

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
        run_git(dir.path(), &["fetch", "origin"]).await.unwrap();

        (dir, remote_dir)
    }

    #[tokio::test]
    async fn create_and_remove_worktree() {
        let (dir, _remote) = init_temp_repo_with_remote().await;

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
        let (dir, _remote) = init_temp_repo_with_remote().await;
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

    #[tokio::test]
    async fn conflict_markers_detected_in_content() {
        let dir = tempfile::tempdir().unwrap();
        let with_markers = "line 1\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\nline 2";
        let without_markers = "line 1\nresolved content\nline 2";

        tokio::fs::write(dir.path().join("conflicted.txt"), with_markers).await.unwrap();
        tokio::fs::write(dir.path().join("resolved.txt"), without_markers).await.unwrap();

        let files = vec!["conflicted.txt".to_string(), "resolved.txt".to_string()];
        let result = files_with_conflict_markers(dir.path(), &files).await;

        assert_eq!(result, vec!["conflicted.txt"]);
    }

    #[tokio::test]
    async fn conflict_markers_empty_when_all_resolved() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "clean content").await.unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "also clean").await.unwrap();

        let files = vec!["a.txt".to_string(), "b.txt".to_string()];
        let result = files_with_conflict_markers(dir.path(), &files).await;

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn conflict_markers_missing_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec!["nonexistent.txt".to_string()];
        let result = files_with_conflict_markers(dir.path(), &files).await;

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn resolved_file_still_unmerged_in_index() {
        // Reproduces the root cause: agent resolves conflict markers in the
        // working tree, but git index still shows the file as Unmerged because
        // `git add` hasn't been run yet.
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

        // Resolve the conflict markers in the working tree (simulating what an agent does)
        tokio::fs::write(dir.path().join("README.md"), "resolved version").await.unwrap();

        // Index-based check still sees the file as Unmerged (the old broken check)
        let index_conflicts = conflicting_files(dir.path()).await;
        assert!(
            !index_conflicts.is_empty(),
            "file should still be Unmerged in git index before git add"
        );

        // Content-based check correctly sees no conflict markers
        let content_conflicts = files_with_conflict_markers(dir.path(), &files).await;
        assert!(
            content_conflicts.is_empty(),
            "file content has no conflict markers, should be empty"
        );

        // Clean up
        abort_rebase(dir.path()).await;
    }

    #[tokio::test]
    async fn is_dirty_detects_modified_files() {
        let dir = init_temp_repo().await;
        assert!(!is_dirty(dir.path()).await.unwrap());

        tokio::fs::write(dir.path().join("README.md"), "modified").await.unwrap();
        assert!(is_dirty(dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn commit_all_commits_tracked_changes_only() {
        let dir = init_temp_repo().await;

        // Untracked file should NOT be staged (git add -u ignores untracked)
        tokio::fs::write(dir.path().join("new.txt"), "new file").await.unwrap();
        // Modified tracked file should be staged
        tokio::fs::write(dir.path().join("README.md"), "modified").await.unwrap();

        let committed = commit_all(dir.path(), "save agent work").await.unwrap();
        assert!(committed);

        let log = run_git(dir.path(), &["log", "--oneline", "-1"]).await.unwrap();
        assert!(log.contains("save agent work"));

        // README.md should be committed, new.txt should still be untracked
        let diff = run_git(dir.path(), &["diff", "HEAD~1", "--name-only"]).await.unwrap();
        assert!(diff.contains("README.md"));
        assert!(!diff.contains("new.txt"));

        // Worktree still dirty because of the untracked file
        assert!(is_dirty(dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn commit_all_returns_false_when_clean() {
        let dir = init_temp_repo().await;
        let committed = commit_all(dir.path(), "nothing to commit").await.unwrap();
        assert!(!committed);
    }

    #[tokio::test]
    async fn commit_all_skips_untracked_only_worktree() {
        let dir = init_temp_repo().await;

        // Only untracked files -- is_dirty sees them but commit_all should skip
        tokio::fs::write(dir.path().join("temp.log"), "agent output").await.unwrap();
        assert!(is_dirty(dir.path()).await.unwrap());

        let committed = commit_all(dir.path(), "should not commit").await.unwrap();
        assert!(!committed);

        // The untracked file should still be there, not committed
        let log = run_git(dir.path(), &["log", "--oneline", "-1"]).await.unwrap();
        assert!(!log.contains("should not commit"));
    }

    #[tokio::test]
    async fn start_rebase_dirty_worktree_returns_failed() {
        let dir = init_temp_repo().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        run_git(dir.path(), &["checkout", "-b", "feature"]).await.unwrap();
        tokio::fs::write(dir.path().join("feature.txt"), "feature work").await.unwrap();
        run_git(dir.path(), &["add", "."]).await.unwrap();
        run_git(dir.path(), &["commit", "-m", "feature commit"]).await.unwrap();

        // Leave uncommitted changes to a tracked file (simulating fixer that didn't commit)
        tokio::fs::write(dir.path().join("README.md"), "uncommitted work").await.unwrap();

        run_git(dir.path(), &["remote", "add", "origin", &dir.path().to_string_lossy()])
            .await
            .unwrap();

        let outcome = start_rebase(dir.path(), &branch).await;
        assert!(
            matches!(outcome, RebaseOutcome::Failed(ref msg) if msg.contains("could not start")),
            "expected Failed with 'could not start' message, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn rebase_in_progress_false_when_clean() {
        let dir = init_temp_repo().await;
        assert!(!rebase_in_progress(dir.path()).await);
    }

    #[tokio::test]
    async fn rebase_in_progress_true_during_conflict() {
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
        assert!(matches!(outcome, RebaseOutcome::RebaseConflicts(_)));
        assert!(rebase_in_progress(dir.path()).await);

        abort_rebase(dir.path()).await;
        assert!(!rebase_in_progress(dir.path()).await);
    }

    #[tokio::test]
    async fn fetch_branch_updates_remote_tracking_ref() {
        let (dir, remote_dir) = init_temp_repo_with_remote().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        let before_sha =
            run_git(dir.path(), &["rev-parse", &format!("origin/{branch}")]).await.unwrap();

        let _other = push_remote_commit(remote_dir.path(), &branch, "remote.txt").await;

        // Remote tracking ref should still be at the old SHA
        let stale_sha =
            run_git(dir.path(), &["rev-parse", &format!("origin/{branch}")]).await.unwrap();
        assert_eq!(before_sha, stale_sha);

        // fetch_branch should update the remote tracking ref
        fetch_branch(dir.path(), &branch).await.unwrap();

        let after_sha =
            run_git(dir.path(), &["rev-parse", &format!("origin/{branch}")]).await.unwrap();
        assert_ne!(before_sha, after_sha, "origin/{branch} should have advanced after fetch");
    }

    #[tokio::test]
    async fn fetch_branch_no_remote_errors() {
        let dir = init_temp_repo().await;
        let result = fetch_branch(dir.path(), "main").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn worktree_after_fetch_includes_remote_changes() {
        let (dir, remote_dir) = init_temp_repo_with_remote().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        let _other = push_remote_commit(remote_dir.path(), &branch, "merged.txt").await;

        fetch_branch(dir.path(), &branch).await.unwrap();

        let wt = create_worktree(dir.path(), 99, &branch).await.unwrap();
        assert!(
            wt.path.join("merged.txt").exists(),
            "worktree should contain the file from the merged PR"
        );
    }

    /// Helper: push a commit from a separate clone so the local repo falls behind.
    async fn push_remote_commit(
        remote_dir: &std::path::Path,
        branch: &str,
        filename: &str,
    ) -> tempfile::TempDir {
        let other = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", &remote_dir.to_string_lossy(), "."])
            .current_dir(other.path())
            .output()
            .await
            .unwrap();
        for args in
            [&["config", "user.email", "test@test.com"][..], &["config", "user.name", "Test"]]
        {
            Command::new("git").args(args).current_dir(other.path()).output().await.unwrap();
        }
        tokio::fs::write(other.path().join(filename), "content").await.unwrap();
        run_git(other.path(), &["add", "."]).await.unwrap();
        run_git(other.path(), &["commit", "-m", &format!("add {filename}")]).await.unwrap();
        run_git(other.path(), &["push", "origin", branch]).await.unwrap();
        other
    }

    #[tokio::test]
    async fn advance_local_branch_when_checked_out() {
        let (dir, remote_dir) = init_temp_repo_with_remote().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        let before = run_git(dir.path(), &["rev-parse", &branch]).await.unwrap();

        let _other = push_remote_commit(remote_dir.path(), &branch, "new.txt").await;
        fetch_branch(dir.path(), &branch).await.unwrap();

        // Local branch should still be behind after fetch
        let after_fetch = run_git(dir.path(), &["rev-parse", &branch]).await.unwrap();
        assert_eq!(before, after_fetch, "local branch should not advance from fetch alone");

        // advance_local_branch should fast-forward it
        advance_local_branch(dir.path(), &branch).await.unwrap();

        let after_advance = run_git(dir.path(), &["rev-parse", &branch]).await.unwrap();
        let remote_sha =
            run_git(dir.path(), &["rev-parse", &format!("origin/{branch}")]).await.unwrap();
        assert_eq!(after_advance, remote_sha, "local branch should match origin after advance");
        assert!(dir.path().join("new.txt").exists(), "working tree should have the new file");
    }

    #[tokio::test]
    async fn advance_local_branch_when_not_checked_out() {
        let (dir, remote_dir) = init_temp_repo_with_remote().await;
        let branch = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap();

        // Switch to a different branch so the base branch is not checked out
        run_git(dir.path(), &["checkout", "-b", "other"]).await.unwrap();

        let before = run_git(dir.path(), &["rev-parse", &branch]).await.unwrap();

        let _other = push_remote_commit(remote_dir.path(), &branch, "new.txt").await;
        fetch_branch(dir.path(), &branch).await.unwrap();

        advance_local_branch(dir.path(), &branch).await.unwrap();

        let after = run_git(dir.path(), &["rev-parse", &branch]).await.unwrap();
        let remote_sha =
            run_git(dir.path(), &["rev-parse", &format!("origin/{branch}")]).await.unwrap();
        assert_ne!(before, after, "local branch should have moved");
        assert_eq!(after, remote_sha, "local branch should match origin after advance");
    }
}
