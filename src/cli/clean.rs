use anyhow::{Context, Result};

use super::{CleanArgs, GlobalOpts};
use crate::{db, git};

pub async fn run(args: CleanArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
    let all = !args.only_logs && !args.only_trees && !args.only_branches;

    if all || args.only_trees {
        let pruned = git::clean_worktrees(&project_dir).await?;
        println!("pruned {pruned} worktree(s)");

        let worktree_dir = project_dir.join(".oven").join("worktrees");
        if worktree_dir.exists() {
            let removed = remove_dir_contents(&worktree_dir)?;
            println!("removed {removed} worktree dir(s)");
        }
    }

    if all || args.only_logs {
        let logs_dir = project_dir.join(".oven").join("logs");
        if logs_dir.exists() {
            let db_path = project_dir.join(".oven").join("oven.db");
            let removed = if db_path.exists() {
                let conn = db::open(&db_path)?;
                remove_completed_logs(&conn, &logs_dir)?
            } else {
                remove_dir_contents(&logs_dir)?
            };
            println!("removed {removed} log dir(s)");
        }
    }

    if all || args.only_branches {
        let base = git::default_branch(&project_dir).await?;
        let branches = git::list_merged_branches(&project_dir, &base).await?;
        let count = branches.len();
        for branch in branches {
            git::delete_branch(&project_dir, &branch).await?;
        }
        println!("deleted {count} merged branch(es)");
    }

    Ok(())
}

fn remove_dir_contents(dir: &std::path::Path) -> Result<u32> {
    let mut count = 0u32;
    for entry in std::fs::read_dir(dir).context("reading directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing {}", path.display()))?;
        } else {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        count += 1;
    }
    Ok(count)
}

fn remove_completed_logs(conn: &rusqlite::Connection, logs_dir: &std::path::Path) -> Result<u32> {
    let completed_runs = db::runs::get_runs_by_status(conn, db::RunStatus::Complete)?;
    let failed_runs = db::runs::get_runs_by_status(conn, db::RunStatus::Failed)?;

    let mut count = 0u32;
    for run in completed_runs.iter().chain(failed_runs.iter()) {
        let log_path = logs_dir.join(&run.id);
        if log_path.exists() {
            std::fs::remove_dir_all(&log_path)
                .with_context(|| format!("removing logs for run {}", run.id))?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_dir_contents_cleans_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let removed = remove_dir_contents(dir.path()).unwrap();
        assert_eq!(removed, 3);
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn remove_dir_contents_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let removed = remove_dir_contents(dir.path()).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn remove_completed_logs_only_removes_finished() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::create_dir(logs_dir.join("run1")).unwrap();
        std::fs::create_dir(logs_dir.join("run2")).unwrap();
        std::fs::create_dir(logs_dir.join("run3")).unwrap();

        let conn = db::open_in_memory().unwrap();
        // Insert runs with different statuses
        db::runs::insert_run(
            &conn,
            &db::Run {
                id: "run1".to_string(),
                issue_number: 1,
                status: db::RunStatus::Complete,
                pr_number: None,
                branch: None,
                worktree_path: None,
                cost_usd: 0.0,
                auto_merge: false,
                started_at: "2026-03-12T00:00:00".to_string(),
                finished_at: None,
                error_message: None,
                complexity: "full".to_string(),
                issue_source: "github".to_string(),
            },
        )
        .unwrap();
        db::runs::insert_run(
            &conn,
            &db::Run {
                id: "run2".to_string(),
                issue_number: 2,
                status: db::RunStatus::Implementing,
                pr_number: None,
                branch: None,
                worktree_path: None,
                cost_usd: 0.0,
                auto_merge: false,
                started_at: "2026-03-12T00:00:00".to_string(),
                finished_at: None,
                error_message: None,
                complexity: "full".to_string(),
                issue_source: "github".to_string(),
            },
        )
        .unwrap();
        db::runs::insert_run(
            &conn,
            &db::Run {
                id: "run3".to_string(),
                issue_number: 3,
                status: db::RunStatus::Failed,
                pr_number: None,
                branch: None,
                worktree_path: None,
                cost_usd: 0.0,
                auto_merge: false,
                started_at: "2026-03-12T00:00:00".to_string(),
                finished_at: None,
                error_message: None,
                complexity: "full".to_string(),
                issue_source: "github".to_string(),
            },
        )
        .unwrap();

        let removed = remove_completed_logs(&conn, &logs_dir).unwrap();
        // run1 (complete) and run3 (failed) should be removed, run2 (implementing) stays
        assert_eq!(removed, 2);
        assert!(!logs_dir.join("run1").exists());
        assert!(logs_dir.join("run2").exists());
        assert!(!logs_dir.join("run3").exists());
    }
}
