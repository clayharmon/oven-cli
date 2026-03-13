mod common;

use oven_cli::db::{self, RunStatus};

#[test]
fn full_run_lifecycle() {
    let conn = common::test_db();

    let run = db::Run {
        id: "test1234".to_string(),
        issue_number: 42,
        status: RunStatus::Pending,
        pr_number: None,
        branch: None,
        worktree_path: None,
        cost_usd: 0.0,
        auto_merge: true,
        started_at: "2026-03-12T10:00:00".to_string(),
        finished_at: None,
        error_message: None,
        complexity: "full".to_string(),
    };

    // Insert
    db::runs::insert_run(&conn, &run).unwrap();
    let retrieved = db::runs::get_run(&conn, "test1234").unwrap().unwrap();
    assert_eq!(retrieved.issue_number, 42);
    assert!(retrieved.auto_merge);

    // Update status through lifecycle
    db::runs::update_run_status(&conn, "test1234", RunStatus::Implementing).unwrap();
    db::runs::update_run_pr(&conn, "test1234", 99).unwrap();
    db::runs::update_run_cost(&conn, "test1234", 3.50).unwrap();

    let updated = db::runs::get_run(&conn, "test1234").unwrap().unwrap();
    assert_eq!(updated.status, RunStatus::Implementing);
    assert_eq!(updated.pr_number, Some(99));
    assert!((updated.cost_usd - 3.50).abs() < f64::EPSILON);

    // Finish
    db::runs::finish_run(&conn, "test1234", RunStatus::Complete, None).unwrap();
    let finished = db::runs::get_run(&conn, "test1234").unwrap().unwrap();
    assert_eq!(finished.status, RunStatus::Complete);
    assert!(finished.finished_at.is_some());
}

#[test]
fn agent_run_with_findings() {
    let conn = common::test_db();

    // Create parent run
    db::runs::insert_run(
        &conn,
        &db::Run {
            id: "run123".to_string(),
            issue_number: 1,
            status: RunStatus::Reviewing,
            pr_number: Some(10),
            branch: Some("oven/issue-1-abc".to_string()),
            worktree_path: None,
            cost_usd: 1.0,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: None,
            error_message: None,
            complexity: "full".to_string(),
        },
    )
    .unwrap();

    // Create agent run
    let ar_id = db::agent_runs::insert_agent_run(
        &conn,
        &db::AgentRun {
            id: 0,
            run_id: "run123".to_string(),
            agent: "reviewer".to_string(),
            cycle: 1,
            status: "running".to_string(),
            cost_usd: 0.0,
            turns: 0,
            started_at: "2026-03-12T10:01:00".to_string(),
            finished_at: None,
            output_summary: None,
            error_message: None,
        },
    )
    .unwrap();

    // Add findings
    db::agent_runs::insert_finding(
        &conn,
        &db::ReviewFinding {
            id: 0,
            agent_run_id: ar_id,
            severity: "critical".to_string(),
            category: "bug".to_string(),
            file_path: Some("src/lib.rs".to_string()),
            line_number: Some(42),
            message: "null pointer".to_string(),
            resolved: false,
        },
    )
    .unwrap();

    db::agent_runs::insert_finding(
        &conn,
        &db::ReviewFinding {
            id: 0,
            agent_run_id: ar_id,
            severity: "info".to_string(),
            category: "style".to_string(),
            file_path: None,
            line_number: None,
            message: "consider renaming".to_string(),
            resolved: false,
        },
    )
    .unwrap();

    // Verify unresolved findings (should exclude info)
    let unresolved = db::agent_runs::get_unresolved_findings(&conn, "run123").unwrap();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].severity, "critical");

    // Resolve the critical finding
    db::agent_runs::resolve_finding(&conn, unresolved[0].id).unwrap();

    let remaining = db::agent_runs::get_unresolved_findings(&conn, "run123").unwrap();
    assert!(remaining.is_empty());
}

#[test]
fn multiple_runs_query_correctly() {
    let conn = common::test_db();

    for i in 1..=5 {
        db::runs::insert_run(
            &conn,
            &db::Run {
                id: format!("run{i:05}"),
                issue_number: i,
                status: if i <= 3 { RunStatus::Complete } else { RunStatus::Pending },
                pr_number: None,
                branch: None,
                worktree_path: None,
                cost_usd: f64::from(i),
                auto_merge: false,
                started_at: format!("2026-03-{i:02}T10:00:00"),
                finished_at: None,
                error_message: None,
                complexity: "full".to_string(),
            },
        )
        .unwrap();
    }

    let all = db::runs::get_all_runs(&conn).unwrap();
    assert_eq!(all.len(), 5);
    // Most recent first
    assert_eq!(all[0].issue_number, 5);

    let complete = db::runs::get_runs_by_status(&conn, RunStatus::Complete).unwrap();
    assert_eq!(complete.len(), 3);

    let pending = db::runs::get_runs_by_status(&conn, RunStatus::Pending).unwrap();
    assert_eq!(pending.len(), 2);

    let latest = db::runs::get_latest_run(&conn).unwrap().unwrap();
    assert_eq!(latest.issue_number, 5);
}
