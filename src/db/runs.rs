use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::{Run, RunStatus};

pub fn insert_run(conn: &Connection, run: &Run) -> Result<()> {
    conn.execute(
        "INSERT INTO runs (id, issue_number, status, pr_number, branch, worktree_path, \
         cost_usd, auto_merge, started_at, finished_at, error_message, complexity) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            run.id,
            run.issue_number,
            run.status.to_string(),
            run.pr_number,
            run.branch,
            run.worktree_path,
            run.cost_usd,
            run.auto_merge,
            run.started_at,
            run.finished_at,
            run.error_message,
            run.complexity,
        ],
    )
    .context("inserting run")?;
    Ok(())
}

pub fn get_run(conn: &Connection, id: &str) -> Result<Option<Run>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, issue_number, status, pr_number, branch, worktree_path, \
             cost_usd, auto_merge, started_at, finished_at, error_message, complexity \
             FROM runs WHERE id = ?1",
        )
        .context("preparing get_run")?;

    let mut rows = stmt.query_map(params![id], row_to_run).context("querying run")?;
    match rows.next() {
        Some(row) => Ok(Some(row.context("reading run row")?)),
        None => Ok(None),
    }
}

pub fn get_runs_by_status(conn: &Connection, status: RunStatus) -> Result<Vec<Run>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, issue_number, status, pr_number, branch, worktree_path, \
             cost_usd, auto_merge, started_at, finished_at, error_message, complexity \
             FROM runs WHERE status = ?1 ORDER BY started_at",
        )
        .context("preparing get_runs_by_status")?;

    let rows = stmt
        .query_map(params![status.to_string()], row_to_run)
        .context("querying runs by status")?;
    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting runs")
}

pub fn get_latest_run(conn: &Connection) -> Result<Option<Run>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, issue_number, status, pr_number, branch, worktree_path, \
             cost_usd, auto_merge, started_at, finished_at, error_message, complexity \
             FROM runs ORDER BY started_at DESC LIMIT 1",
        )
        .context("preparing get_latest_run")?;

    let mut rows = stmt.query_map([], row_to_run).context("querying latest run")?;
    match rows.next() {
        Some(row) => Ok(Some(row.context("reading latest run row")?)),
        None => Ok(None),
    }
}

pub fn get_all_runs(conn: &Connection) -> Result<Vec<Run>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, issue_number, status, pr_number, branch, worktree_path, \
             cost_usd, auto_merge, started_at, finished_at, error_message, complexity \
             FROM runs ORDER BY started_at DESC",
        )
        .context("preparing get_all_runs")?;

    let rows = stmt.query_map([], row_to_run).context("querying all runs")?;
    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting all runs")
}

pub fn update_run_status(conn: &Connection, id: &str, status: RunStatus) -> Result<()> {
    conn.execute("UPDATE runs SET status = ?1 WHERE id = ?2", params![status.to_string(), id])
        .context("updating run status")?;
    Ok(())
}

pub fn update_run_pr(conn: &Connection, id: &str, pr_number: u32) -> Result<()> {
    conn.execute("UPDATE runs SET pr_number = ?1 WHERE id = ?2", params![pr_number, id])
        .context("updating run PR number")?;
    Ok(())
}

pub fn update_run_cost(conn: &Connection, id: &str, cost_usd: f64) -> Result<()> {
    conn.execute("UPDATE runs SET cost_usd = ?1 WHERE id = ?2", params![cost_usd, id])
        .context("updating run cost")?;
    Ok(())
}

pub fn finish_run(
    conn: &Connection,
    id: &str,
    status: RunStatus,
    error_message: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE runs SET status = ?1, finished_at = datetime('now'), error_message = ?2 \
         WHERE id = ?3",
        params![status.to_string(), error_message, id],
    )
    .context("finishing run")?;
    Ok(())
}

pub fn update_run_complexity(conn: &Connection, id: &str, complexity: &str) -> Result<()> {
    conn.execute("UPDATE runs SET complexity = ?1 WHERE id = ?2", params![complexity, id])
        .context("updating run complexity")?;
    Ok(())
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    let status_str: String = row.get(2)?;
    let status: RunStatus = status_str.parse().map_err(|_| {
        rusqlite::Error::InvalidColumnType(2, "status".to_string(), rusqlite::types::Type::Text)
    })?;
    Ok(Run {
        id: row.get(0)?,
        issue_number: row.get(1)?,
        status,
        pr_number: row.get(3)?,
        branch: row.get(4)?,
        worktree_path: row.get(5)?,
        cost_usd: row.get(6)?,
        auto_merge: row.get(7)?,
        started_at: row.get(8)?,
        finished_at: row.get(9)?,
        error_message: row.get(10)?,
        complexity: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn test_db() -> Connection {
        db::open_in_memory().unwrap()
    }

    fn sample_run(id: &str, issue: u32) -> Run {
        Run {
            id: id.to_string(),
            issue_number: issue,
            status: RunStatus::Pending,
            pr_number: None,
            branch: None,
            worktree_path: None,
            cost_usd: 0.0,
            auto_merge: false,
            started_at: "2026-03-12T00:00:00".to_string(),
            finished_at: None,
            error_message: None,
            complexity: "full".to_string(),
        }
    }

    #[test]
    fn insert_and_get_run() {
        let conn = test_db();
        let run = sample_run("abcd1234", 42);
        insert_run(&conn, &run).unwrap();

        let retrieved = get_run(&conn, "abcd1234").unwrap().unwrap();
        assert_eq!(retrieved.id, "abcd1234");
        assert_eq!(retrieved.issue_number, 42);
        assert_eq!(retrieved.status, RunStatus::Pending);
    }

    #[test]
    fn get_nonexistent_run_returns_none() {
        let conn = test_db();
        assert!(get_run(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn update_status() {
        let conn = test_db();
        insert_run(&conn, &sample_run("abcd1234", 42)).unwrap();

        update_run_status(&conn, "abcd1234", RunStatus::Implementing).unwrap();
        let run = get_run(&conn, "abcd1234").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Implementing);
    }

    #[test]
    fn update_pr_number() {
        let conn = test_db();
        insert_run(&conn, &sample_run("abcd1234", 42)).unwrap();

        update_run_pr(&conn, "abcd1234", 99).unwrap();
        let run = get_run(&conn, "abcd1234").unwrap().unwrap();
        assert_eq!(run.pr_number, Some(99));
    }

    #[test]
    fn update_cost() {
        let conn = test_db();
        insert_run(&conn, &sample_run("abcd1234", 42)).unwrap();

        update_run_cost(&conn, "abcd1234", 3.50).unwrap();
        let run = get_run(&conn, "abcd1234").unwrap().unwrap();
        assert!((run.cost_usd - 3.50).abs() < f64::EPSILON);
    }

    #[test]
    fn finish_run_sets_status_and_timestamp() {
        let conn = test_db();
        insert_run(&conn, &sample_run("abcd1234", 42)).unwrap();

        finish_run(&conn, "abcd1234", RunStatus::Complete, None).unwrap();
        let run = get_run(&conn, "abcd1234").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Complete);
        assert!(run.finished_at.is_some());
    }

    #[test]
    fn finish_run_with_error() {
        let conn = test_db();
        insert_run(&conn, &sample_run("abcd1234", 42)).unwrap();

        finish_run(&conn, "abcd1234", RunStatus::Failed, Some("boom")).unwrap();
        let run = get_run(&conn, "abcd1234").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error_message.as_deref(), Some("boom"));
    }

    #[test]
    fn get_runs_by_status_filters() {
        let conn = test_db();
        insert_run(&conn, &sample_run("aaa11111", 1)).unwrap();
        insert_run(&conn, &sample_run("bbb22222", 2)).unwrap();

        update_run_status(&conn, "aaa11111", RunStatus::Complete).unwrap();

        let pending = get_runs_by_status(&conn, RunStatus::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "bbb22222");

        let complete = get_runs_by_status(&conn, RunStatus::Complete).unwrap();
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0].id, "aaa11111");
    }

    #[test]
    fn get_latest_run_returns_most_recent() {
        let conn = test_db();
        let mut run1 = sample_run("aaa11111", 1);
        run1.started_at = "2026-03-01T00:00:00".to_string();
        let mut run2 = sample_run("bbb22222", 2);
        run2.started_at = "2026-03-02T00:00:00".to_string();

        insert_run(&conn, &run1).unwrap();
        insert_run(&conn, &run2).unwrap();

        let latest = get_latest_run(&conn).unwrap().unwrap();
        assert_eq!(latest.id, "bbb22222");
    }

    #[test]
    fn get_all_runs_returns_ordered() {
        let conn = test_db();
        let mut run1 = sample_run("aaa11111", 1);
        run1.started_at = "2026-03-01T00:00:00".to_string();
        let mut run2 = sample_run("bbb22222", 2);
        run2.started_at = "2026-03-02T00:00:00".to_string();

        insert_run(&conn, &run1).unwrap();
        insert_run(&conn, &run2).unwrap();

        let all = get_all_runs(&conn).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "bbb22222"); // most recent first
    }
}
