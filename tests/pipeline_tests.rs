mod common;

use oven_cli::{
    agents::{Severity, parse_review_output},
    db::{self, RunStatus},
    pipeline::executor::generate_run_id,
};

// -- State machine integration tests --

#[test]
fn full_happy_path_state_transitions() {
    // Simulate: Pending -> Implementing -> Reviewing (clean) -> Merging -> Complete
    let mut status = RunStatus::Pending;

    status = status.next(false, 0); // start
    assert_eq!(status, RunStatus::Implementing);

    status = status.next(false, 0); // implement done -> review
    assert_eq!(status, RunStatus::Reviewing);

    status = status.next(false, 1); // clean review -> merge
    assert_eq!(status, RunStatus::Merging);

    status = status.next(false, 0); // merge done -> complete
    assert_eq!(status, RunStatus::Complete);

    assert!(status.is_terminal());
}

#[test]
fn one_fix_cycle_path() {
    let mut status = RunStatus::Pending;

    status = status.next(false, 0); // -> Implementing
    status = status.next(false, 0); // -> Reviewing

    // First review finds issues
    status = status.next(true, 1); // -> Fixing (cycle 1 < 2)
    assert_eq!(status, RunStatus::Fixing);

    status = status.next(false, 1); // -> Reviewing again
    assert_eq!(status, RunStatus::Reviewing);

    // Second review is clean
    status = status.next(false, 2); // -> Merging
    assert_eq!(status, RunStatus::Merging);

    status = status.next(false, 0); // -> Complete
    assert_eq!(status, RunStatus::Complete);
}

#[test]
fn max_fix_cycles_path() {
    let mut status = RunStatus::Pending;

    status = status.next(false, 0); // -> Implementing
    status = status.next(false, 0); // -> Reviewing

    // Cycle 1: findings -> fix -> review
    status = status.next(true, 1); // -> Fixing
    assert_eq!(status, RunStatus::Fixing);
    status = status.next(false, 1); // -> Reviewing
    assert_eq!(status, RunStatus::Reviewing);

    // Cycle 2: still findings -> Failed (max exceeded)
    status = status.next(true, 2); // -> Failed
    assert_eq!(status, RunStatus::Failed);
    assert!(status.is_terminal());
}

// -- DB integration tests --

#[test]
fn run_and_agent_run_cost_aggregation() {
    let conn = common::test_db();

    db::runs::insert_run(
        &conn,
        &db::Run {
            id: "cost0001".to_string(),
            issue_number: 1,
            status: RunStatus::Implementing,
            pr_number: Some(10),
            branch: Some("oven/issue-1-abc".to_string()),
            worktree_path: None,
            cost_usd: 0.0,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: None,
            error_message: None,
        },
    )
    .unwrap();

    // Add multiple agent runs with costs
    let agents = [("implementer", 2.50), ("reviewer", 0.85), ("fixer", 0.73), ("reviewer", 0.45)];
    let mut total = 0.0;

    for (agent, cost) in &agents {
        let ar_id = db::agent_runs::insert_agent_run(
            &conn,
            &db::AgentRun {
                id: 0,
                run_id: "cost0001".to_string(),
                agent: agent.to_string(),
                cycle: 1,
                status: "complete".to_string(),
                cost_usd: *cost,
                turns: 5,
                started_at: "2026-03-12T10:01:00".to_string(),
                finished_at: Some("2026-03-12T10:02:00".to_string()),
                output_summary: None,
                error_message: None,
            },
        )
        .unwrap();
        assert!(ar_id > 0);

        total += cost;
        db::runs::update_run_cost(&conn, "cost0001", total).unwrap();
    }

    let run = db::runs::get_run(&conn, "cost0001").unwrap().unwrap();
    assert!((run.cost_usd - 4.53).abs() < f64::EPSILON);

    let agent_runs = db::agent_runs::get_agent_runs_for_run(&conn, "cost0001").unwrap();
    assert_eq!(agent_runs.len(), 4);
}

// -- Review output parsing integration tests --

#[test]
fn review_output_with_mixed_severities() {
    let json = r#"{
        "findings": [
            {"severity": "critical", "category": "security", "file_path": "src/auth.rs", "line_number": 15, "message": "SQL injection"},
            {"severity": "warning", "category": "perf", "message": "unnecessary clone"},
            {"severity": "info", "category": "style", "message": "consider renaming"}
        ],
        "summary": "3 findings"
    }"#;

    let output = parse_review_output(json).unwrap();
    assert_eq!(output.findings.len(), 3);

    let critical: Vec<_> =
        output.findings.iter().filter(|f| f.severity == Severity::Critical).collect();
    assert_eq!(critical.len(), 1);
    assert_eq!(critical[0].file_path.as_deref(), Some("src/auth.rs"));
    assert_eq!(critical[0].line_number, Some(15));

    let actionable: Vec<_> =
        output.findings.iter().filter(|f| f.severity != Severity::Info).collect();
    assert_eq!(actionable.len(), 2);
}

#[test]
fn review_output_empty_findings_array() {
    let json = r#"{"findings": [], "summary": "all clean"}"#;
    let output = parse_review_output(json).unwrap();
    assert!(output.findings.is_empty());
    assert_eq!(output.summary, "all clean");
}

#[test]
fn review_output_with_extra_fields_is_forward_compatible() {
    let json = r#"{
        "findings": [{"severity": "warning", "category": "bug", "message": "issue", "confidence": 0.95, "suggested_fix": "do this"}],
        "summary": "ok",
        "metadata": {"version": "2.0"}
    }"#;
    let output = parse_review_output(json).unwrap();
    assert_eq!(output.findings.len(), 1);
}

// -- Run ID tests --

#[test]
fn run_ids_are_unique_across_batch() {
    let ids: Vec<String> = (0..1000).map(|_| generate_run_id()).collect();
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len());
}

#[test]
fn run_ids_contain_only_hex() {
    for _ in 0..100 {
        let id = generate_run_id();
        assert_eq!(id.len(), 8, "run ID should be 8 chars: {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "run ID should be hex only: {id}");
    }
}
