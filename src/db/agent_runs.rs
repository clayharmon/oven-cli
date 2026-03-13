use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::{AgentRun, ReviewFinding};

pub fn insert_agent_run(conn: &Connection, agent_run: &AgentRun) -> Result<i64> {
    conn.execute(
        "INSERT INTO agent_runs (run_id, agent, cycle, status, cost_usd, turns, \
         started_at, finished_at, output_summary, error_message, raw_output) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            agent_run.run_id,
            agent_run.agent,
            agent_run.cycle,
            agent_run.status,
            agent_run.cost_usd,
            agent_run.turns,
            agent_run.started_at,
            agent_run.finished_at,
            agent_run.output_summary,
            agent_run.error_message,
            agent_run.raw_output,
        ],
    )
    .context("inserting agent run")?;
    Ok(conn.last_insert_rowid())
}

pub fn get_agent_runs_for_run(conn: &Connection, run_id: &str) -> Result<Vec<AgentRun>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, run_id, agent, cycle, status, cost_usd, turns, \
             started_at, finished_at, output_summary, error_message, raw_output \
             FROM agent_runs WHERE run_id = ?1 ORDER BY id",
        )
        .context("preparing get_agent_runs_for_run")?;

    let rows = stmt
        .query_map(params![run_id], |row| {
            Ok(AgentRun {
                id: row.get(0)?,
                run_id: row.get(1)?,
                agent: row.get(2)?,
                cycle: row.get(3)?,
                status: row.get(4)?,
                cost_usd: row.get(5)?,
                turns: row.get(6)?,
                started_at: row.get(7)?,
                finished_at: row.get(8)?,
                output_summary: row.get(9)?,
                error_message: row.get(10)?,
                raw_output: row.get(11)?,
            })
        })
        .context("querying agent runs")?;

    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting agent runs")
}

#[allow(clippy::too_many_arguments)]
pub fn finish_agent_run(
    conn: &Connection,
    id: i64,
    status: &str,
    cost_usd: f64,
    turns: u32,
    output_summary: Option<&str>,
    error_message: Option<&str>,
    raw_output: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET status = ?1, cost_usd = ?2, turns = ?3, \
         finished_at = datetime('now'), output_summary = ?4, error_message = ?5, \
         raw_output = ?6 WHERE id = ?7",
        params![status, cost_usd, turns, output_summary, error_message, raw_output, id],
    )
    .context("finishing agent run")?;
    Ok(())
}

pub fn insert_finding(conn: &Connection, finding: &ReviewFinding) -> Result<i64> {
    conn.execute(
        "INSERT INTO review_findings (agent_run_id, severity, category, file_path, \
         line_number, message, resolved) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            finding.agent_run_id,
            finding.severity,
            finding.category,
            finding.file_path,
            finding.line_number,
            finding.message,
            finding.resolved,
        ],
    )
    .context("inserting finding")?;
    Ok(conn.last_insert_rowid())
}

pub fn get_findings_for_agent_run(
    conn: &Connection,
    agent_run_id: i64,
) -> Result<Vec<ReviewFinding>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, agent_run_id, severity, category, file_path, line_number, \
             message, resolved FROM review_findings WHERE agent_run_id = ?1",
        )
        .context("preparing get_findings_for_agent_run")?;

    let rows =
        stmt.query_map(params![agent_run_id], row_to_finding).context("querying findings")?;

    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting findings")
}

pub fn get_unresolved_findings(conn: &Connection, run_id: &str) -> Result<Vec<ReviewFinding>> {
    let mut stmt = conn
        .prepare(
            "SELECT f.id, f.agent_run_id, f.severity, f.category, f.file_path, \
             f.line_number, f.message, f.resolved \
             FROM review_findings f \
             JOIN agent_runs a ON f.agent_run_id = a.id \
             WHERE a.run_id = ?1 AND f.resolved = 0 AND f.severity != 'info'",
        )
        .context("preparing get_unresolved_findings")?;

    let rows =
        stmt.query_map(params![run_id], row_to_finding).context("querying unresolved findings")?;

    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting unresolved findings")
}

pub fn resolve_finding(conn: &Connection, finding_id: i64) -> Result<()> {
    conn.execute("UPDATE review_findings SET resolved = 1 WHERE id = ?1", params![finding_id])
        .context("resolving finding")?;
    Ok(())
}

fn row_to_finding(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewFinding> {
    Ok(ReviewFinding {
        id: row.get(0)?,
        agent_run_id: row.get(1)?,
        severity: row.get(2)?,
        category: row.get(3)?,
        file_path: row.get(4)?,
        line_number: row.get(5)?,
        message: row.get(6)?,
        resolved: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, RunStatus, runs};

    fn test_db() -> Connection {
        db::open_in_memory().unwrap()
    }

    fn insert_test_run(conn: &Connection, id: &str) {
        runs::insert_run(
            conn,
            &db::Run {
                id: id.to_string(),
                issue_number: 1,
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
                issue_source: "github".to_string(),
            },
        )
        .unwrap();
    }

    fn sample_agent_run(run_id: &str, agent: &str) -> AgentRun {
        AgentRun {
            id: 0,
            run_id: run_id.to_string(),
            agent: agent.to_string(),
            cycle: 1,
            status: "running".to_string(),
            cost_usd: 0.0,
            turns: 0,
            started_at: "2026-03-12T00:00:00".to_string(),
            finished_at: None,
            output_summary: None,
            error_message: None,
            raw_output: None,
        }
    }

    #[test]
    fn insert_and_get_agent_run() {
        let conn = test_db();
        insert_test_run(&conn, "run1");

        let ar = sample_agent_run("run1", "implementer");
        let id = insert_agent_run(&conn, &ar).unwrap();
        assert!(id > 0);

        let runs = get_agent_runs_for_run(&conn, "run1").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].agent, "implementer");
    }

    #[test]
    fn finish_agent_run_updates_fields() {
        let conn = test_db();
        insert_test_run(&conn, "run1");
        let id = insert_agent_run(&conn, &sample_agent_run("run1", "reviewer")).unwrap();

        finish_agent_run(&conn, id, "complete", 1.50, 8, Some("all good"), None, None).unwrap();

        let runs = get_agent_runs_for_run(&conn, "run1").unwrap();
        assert_eq!(runs[0].status, "complete");
        assert!((runs[0].cost_usd - 1.50).abs() < f64::EPSILON);
        assert_eq!(runs[0].turns, 8);
        assert_eq!(runs[0].output_summary.as_deref(), Some("all good"));
        assert!(runs[0].finished_at.is_some());
    }

    #[test]
    fn insert_and_get_findings() {
        let conn = test_db();
        insert_test_run(&conn, "run1");
        let ar_id = insert_agent_run(&conn, &sample_agent_run("run1", "reviewer")).unwrap();

        let finding = ReviewFinding {
            id: 0,
            agent_run_id: ar_id,
            severity: "critical".to_string(),
            category: "bug".to_string(),
            file_path: Some("src/main.rs".to_string()),
            line_number: Some(42),
            message: "null pointer".to_string(),
            resolved: false,
        };
        let fid = insert_finding(&conn, &finding).unwrap();
        assert!(fid > 0);

        let findings = get_findings_for_agent_run(&conn, ar_id).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, "critical");
        assert_eq!(findings[0].message, "null pointer");
    }

    #[test]
    fn resolve_finding_updates_flag() {
        let conn = test_db();
        insert_test_run(&conn, "run1");
        let ar_id = insert_agent_run(&conn, &sample_agent_run("run1", "reviewer")).unwrap();

        let finding = ReviewFinding {
            id: 0,
            agent_run_id: ar_id,
            severity: "warning".to_string(),
            category: "style".to_string(),
            file_path: None,
            line_number: None,
            message: "missing docs".to_string(),
            resolved: false,
        };
        let fid = insert_finding(&conn, &finding).unwrap();

        resolve_finding(&conn, fid).unwrap();

        let findings = get_findings_for_agent_run(&conn, ar_id).unwrap();
        assert!(findings[0].resolved);
    }

    #[test]
    fn get_unresolved_findings_filters() {
        let conn = test_db();
        insert_test_run(&conn, "run1");
        let ar_id = insert_agent_run(&conn, &sample_agent_run("run1", "reviewer")).unwrap();

        // Critical - unresolved
        insert_finding(
            &conn,
            &ReviewFinding {
                id: 0,
                agent_run_id: ar_id,
                severity: "critical".to_string(),
                category: "bug".to_string(),
                file_path: None,
                line_number: None,
                message: "bad".to_string(),
                resolved: false,
            },
        )
        .unwrap();

        // Info - should be excluded
        insert_finding(
            &conn,
            &ReviewFinding {
                id: 0,
                agent_run_id: ar_id,
                severity: "info".to_string(),
                category: "note".to_string(),
                file_path: None,
                line_number: None,
                message: "fyi".to_string(),
                resolved: false,
            },
        )
        .unwrap();

        // Warning - resolved, should be excluded
        let wid = insert_finding(
            &conn,
            &ReviewFinding {
                id: 0,
                agent_run_id: ar_id,
                severity: "warning".to_string(),
                category: "style".to_string(),
                file_path: None,
                line_number: None,
                message: "meh".to_string(),
                resolved: false,
            },
        )
        .unwrap();
        resolve_finding(&conn, wid).unwrap();

        let unresolved = get_unresolved_findings(&conn, "run1").unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message, "bad");
    }

    #[test]
    fn raw_output_round_trips() {
        let conn = test_db();
        insert_test_run(&conn, "run1");
        let id = insert_agent_run(&conn, &sample_agent_run("run1", "implementer")).unwrap();

        let raw = r#"{"batches":[{"batch":1,"issues":[]}]}"#;
        finish_agent_run(&conn, id, "complete", 0.5, 3, Some("ok"), None, Some(raw)).unwrap();

        let runs = get_agent_runs_for_run(&conn, "run1").unwrap();
        assert_eq!(runs[0].raw_output.as_deref(), Some(raw));
    }

    #[test]
    fn cascade_delete_removes_agent_runs_and_findings() {
        let conn = test_db();
        insert_test_run(&conn, "run1");
        let ar_id = insert_agent_run(&conn, &sample_agent_run("run1", "reviewer")).unwrap();
        insert_finding(
            &conn,
            &ReviewFinding {
                id: 0,
                agent_run_id: ar_id,
                severity: "critical".to_string(),
                category: "bug".to_string(),
                file_path: None,
                line_number: None,
                message: "bad".to_string(),
                resolved: false,
            },
        )
        .unwrap();

        // Delete the run
        conn.execute("DELETE FROM runs WHERE id = ?1", params!["run1"]).unwrap();

        // Agent runs and findings should be gone
        let agent_runs = get_agent_runs_for_run(&conn, "run1").unwrap();
        assert!(agent_runs.is_empty());

        let findings = get_findings_for_agent_run(&conn, ar_id).unwrap();
        assert!(findings.is_empty());
    }
}
