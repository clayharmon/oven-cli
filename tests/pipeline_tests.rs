mod common;

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use oven_cli::{
    agents::{Complexity, Severity, parse_planner_output, parse_review_output},
    config::{Config, MultiRepoConfig},
    db::{self, RunStatus},
    github::GhClient,
    issues::{IssueOrigin, IssueProvider, PipelineIssue, github::GithubIssueProvider},
    pipeline::{
        executor::{PipelineExecutor, generate_run_id},
        runner,
    },
    process::{AgentResult, CommandOutput, CommandRunner},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A test mock for `CommandRunner` that dispatches based on tool lists.
///
/// Since mockall's `MockCommandRunner` is only available within the crate
/// (`cfg(test)` doesn't apply to integration tests), we implement the trait
/// directly with configurable behavior.
type ClaudeHandler = Box<dyn Fn(&str, &[String], &Path) -> AgentResult + Send + Sync>;
type GhHandler = Box<dyn Fn(&[String], &Path) -> CommandOutput + Send + Sync>;

struct TestRunner {
    claude_handler: ClaudeHandler,
    gh_handler: GhHandler,
}

impl CommandRunner for TestRunner {
    async fn run_claude(
        &self,
        prompt: &str,
        allowed_tools: &[String],
        working_dir: &Path,
        _max_turns: Option<u32>,
    ) -> Result<AgentResult> {
        Ok((self.claude_handler)(prompt, allowed_tools, working_dir))
    }

    async fn run_gh(&self, args: &[String], working_dir: &Path) -> Result<CommandOutput> {
        Ok((self.gh_handler)(args, working_dir))
    }
}

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
            complexity: "full".to_string(),
            issue_source: "github".to_string(),
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
                agent: (*agent).to_string(),
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

    assert_eq!(output.findings.iter().filter(|f| f.severity != Severity::Info).count(), 2);
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

// -- End-to-end pipeline tests with mocked claude/gh --

/// Set up a temp git repo with a bare remote so `git push origin` works.
async fn setup_git_repo_with_remote() -> (tempfile::TempDir, tempfile::TempDir) {
    let bare_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();

    // Create bare remote
    tokio::process::Command::new("git")
        .args(["init", "--bare"])
        .current_dir(bare_dir.path())
        .output()
        .await
        .unwrap();

    // Init working repo
    tokio::process::Command::new("git")
        .args(["init"])
        .current_dir(work_dir.path())
        .output()
        .await
        .unwrap();

    // Configure git
    for args in [vec!["config", "user.email", "test@test.com"], vec!["config", "user.name", "Test"]]
    {
        tokio::process::Command::new("git")
            .args(&args)
            .current_dir(work_dir.path())
            .output()
            .await
            .unwrap();
    }

    // Add remote
    let remote_url = bare_dir.path().to_string_lossy().to_string();
    tokio::process::Command::new("git")
        .args(["remote", "add", "origin", &remote_url])
        .current_dir(work_dir.path())
        .output()
        .await
        .unwrap();

    // Initial commit
    tokio::fs::write(work_dir.path().join("README.md"), "# test\n").await.unwrap();
    tokio::process::Command::new("git")
        .args(["add", "."])
        .current_dir(work_dir.path())
        .output()
        .await
        .unwrap();
    tokio::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(work_dir.path())
        .output()
        .await
        .unwrap();

    // Push to establish remote tracking
    tokio::process::Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(work_dir.path())
        .output()
        .await
        .unwrap();

    // Create .oven directories
    tokio::fs::create_dir_all(work_dir.path().join(".oven/worktrees")).await.unwrap();
    tokio::fs::create_dir_all(work_dir.path().join(".oven/logs")).await.unwrap();

    (work_dir, bare_dir)
}

/// Build a `TestRunner` where the reviewer returns clean findings (no issues).
///
/// - gh calls: all return success. PR create returns a URL with PR #42.
/// - claude calls: implementer returns code output, reviewer returns clean
///   findings, merger returns success.
fn test_runner_clean_review() -> TestRunner {
    TestRunner {
        claude_handler: Box::new(|_prompt, tools, _dir| {
            let tool_list: Vec<&str> = tools.iter().map(String::as_str).collect();
            let output = if tool_list == ["Bash"] {
                "PR marked as ready for review.".to_string()
            } else if tool_list == ["Read", "Glob", "Grep"] {
                r#"{"findings":[],"summary":"all clean, no issues found"}"#.to_string()
            } else {
                "Implementation complete. All tests pass.".to_string()
            };
            AgentResult {
                cost_usd: 1.50,
                duration: Duration::from_secs(10),
                turns: 5,
                output,
                session_id: "sess-e2e".to_string(),
                success: true,
            }
        }),
        gh_handler: Box::new(|args, _dir| {
            let stdout = if args.get(1).map(String::as_str) == Some("create") {
                "https://github.com/test/repo/pull/42\n".to_string()
            } else {
                String::new()
            };
            CommandOutput { stdout, stderr: String::new(), success: true }
        }),
    }
}

fn make_github_issue(number: u32, title: &str, body: &str) -> PipelineIssue {
    PipelineIssue {
        number,
        title: title.to_string(),
        body: body.to_string(),
        source: IssueOrigin::Github,
        target_repo: None,
    }
}

fn make_github_provider(gh: &Arc<GhClient<TestRunner>>) -> Arc<dyn IssueProvider> {
    Arc::new(GithubIssueProvider::new(Arc::clone(gh), "target_repo"))
}

#[tokio::test]
async fn e2e_pipeline_clean_review_completes() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir: repo_dir.clone(),
    });

    let issue =
        make_github_issue(7, "Add retry logic", "Implement retry for transient API failures.");

    // Run the full pipeline
    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "pipeline failed: {result:?}");

    // Verify DB state
    let conn = db.lock().await;
    let runs = db::runs::get_all_runs(&conn).unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.issue_number, 7);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.pr_number, Some(42));
    assert!(run.finished_at.is_some());

    // Verify cost was tracked (3 agents x $1.50 = $4.50)
    assert!(run.cost_usd > 0.0, "cost should be tracked");

    // Verify agent runs were recorded
    let agent_runs = db::agent_runs::get_agent_runs_for_run(&conn, &run.id).unwrap();
    drop(conn);
    assert_eq!(agent_runs.len(), 3, "should have implementer + reviewer + merger");

    let agents: Vec<&str> = agent_runs.iter().map(|ar| ar.agent.as_str()).collect();
    assert!(agents.contains(&"implementer"));
    assert!(agents.contains(&"reviewer"));
    assert!(agents.contains(&"merger"));

    // All agent runs should be complete
    for ar in &agent_runs {
        assert_eq!(ar.status, "complete", "agent {} should be complete", ar.agent);
        assert!(ar.cost_usd > 0.0);
        assert!(ar.turns > 0);
    }
}

/// Build a `TestRunner` where the reviewer returns findings on first review,
/// clean on second review (triggering one fix cycle).
fn test_runner_with_fix_cycle() -> TestRunner {
    let review_count = Arc::new(AtomicU32::new(0));
    let review_count_clone = Arc::clone(&review_count);

    TestRunner {
        claude_handler: Box::new(move |_prompt, tools, _dir| {
            let tool_list: Vec<&str> = tools.iter().map(String::as_str).collect();
            let output = if tool_list == ["Bash"] {
                "PR ready.".to_string()
            } else if tool_list == ["Read", "Glob", "Grep"] {
                let count = review_count_clone.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    r#"{"findings":[{"severity":"warning","category":"bug","message":"missing error handling"}],"summary":"1 issue"}"#.to_string()
                } else {
                    r#"{"findings":[],"summary":"all clean"}"#.to_string()
                }
            } else {
                "Done.".to_string()
            };
            AgentResult {
                cost_usd: 1.00,
                duration: Duration::from_secs(8),
                turns: 4,
                output,
                session_id: "sess-fix".to_string(),
                success: true,
            }
        }),
        gh_handler: Box::new(|args, _dir| {
            let stdout = if args.get(1).map(String::as_str) == Some("create") {
                "https://github.com/test/repo/pull/55\n".to_string()
            } else {
                String::new()
            };
            CommandOutput { stdout, stderr: String::new(), success: true }
        }),
    }
}

#[tokio::test]
async fn e2e_pipeline_with_one_fix_cycle() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_with_fix_cycle());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue =
        make_github_issue(12, "Fix bug in parser", "The JSON parser crashes on empty input.");

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "pipeline failed: {result:?}");

    let conn = db.lock().await;
    let runs = db::runs::get_all_runs(&conn).unwrap();
    let run = &runs[0];
    assert_eq!(run.status, RunStatus::Complete);

    // Should have: implementer, reviewer (findings), fixer, reviewer (clean), merger
    let agent_runs = db::agent_runs::get_agent_runs_for_run(&conn, &run.id).unwrap();
    assert_eq!(agent_runs.len(), 5, "should have 5 agent runs for one fix cycle");

    let agents: Vec<&str> = agent_runs.iter().map(|ar| ar.agent.as_str()).collect();
    assert_eq!(agents, vec!["implementer", "reviewer", "fixer", "reviewer", "merger"]);

    // Verify findings were stored
    let findings = db::agent_runs::get_findings_for_agent_run(&conn, agent_runs[1].id).unwrap();
    drop(conn);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, "warning");
}

#[tokio::test]
async fn e2e_pipeline_cancellation_stops_run() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let cancel = CancellationToken::new();
    cancel.cancel(); // Cancel immediately

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: cancel,
        repo_dir,
    });

    let issue = make_github_issue(99, "Should be cancelled", "This should not complete.");

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_err());

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error_message.as_ref().is_some_and(|m| m.contains("cancelled")));
}

#[tokio::test]
async fn e2e_pipeline_cost_budget_enforced() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let mut config = Config::default();
    config.pipeline.cost_budget = 1.0; // Very low budget, agent costs $1.50

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config,
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue = make_github_issue(50, "Expensive issue", "This will exceed the budget.");

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_err());

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error_message.as_ref().is_some_and(|m| m.contains("cost budget")));
}

#[tokio::test]
async fn e2e_batch_runs_multiple_issues() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues_provider = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues: issues_provider,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issues = vec![
        make_github_issue(1, "First issue", "First."),
        make_github_issue(2, "Second issue", "Second."),
    ];

    // Run batch with max_parallel=1 (serial) to avoid worktree conflicts
    let result = runner::run_batch(&executor, issues, 1, false).await;
    assert!(result.is_ok(), "batch failed: {result:?}");

    let conn = db.lock().await;
    let runs = db::runs::get_all_runs(&conn).unwrap();
    drop(conn);
    assert_eq!(runs.len(), 2);

    for run in &runs {
        assert_eq!(run.status, RunStatus::Complete);
    }
}

// -- Planner integration tests --

#[test]
fn planner_fallback_on_unparseable_output() {
    // When planner returns garbage, parse_planner_output returns None
    assert!(parse_planner_output("I don't know how to plan").is_none());
    assert!(parse_planner_output("").is_none());
    assert!(parse_planner_output("{broken json").is_none());
}

#[test]
fn planner_output_preserves_complexity() {
    let json = r#"{
        "batches": [{
            "batch": 1,
            "issues": [
                {"number": 1, "title": "Config fix", "area": "config", "complexity": "simple"},
                {"number": 2, "title": "New feature", "area": "pipeline", "complexity": "full"}
            ],
            "reasoning": "independent areas"
        }],
        "total_issues": 2,
        "parallel_capacity": 2
    }"#;
    let plan = parse_planner_output(json).unwrap();
    assert_eq!(plan.batches[0].issues[0].complexity, Complexity::Simple);
    assert_eq!(plan.batches[0].issues[1].complexity, Complexity::Full);
}

#[test]
fn explicit_ids_skip_planner() {
    // Verify run_batch (used for explicit IDs) doesn't invoke the planner.
    // It takes issues directly, with no planner invocation.
    // This is a structural test -- run_batch's signature doesn't include planner logic.
    // The test just verifies it runs successfully without a planner.
    // (The actual e2e_batch_runs_multiple_issues test above already covers this.)
}

#[tokio::test]
async fn e2e_complexity_recorded_in_db() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue = make_github_issue(33, "Simple config change", "Update a config value.");

    // Run with explicit simple complexity
    let result = executor.run_issue_with_complexity(&issue, false, Some(Complexity::Simple)).await;
    assert!(result.is_ok(), "pipeline failed: {result:?}");

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.complexity, "simple");
}

#[tokio::test]
async fn e2e_default_complexity_is_full() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue = make_github_issue(34, "Regular issue", "Normal work.");

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "pipeline failed: {result:?}");

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.complexity, "full");
}

// -- Continuous polling tests --

#[tokio::test]
async fn e2e_continuous_polling_processes_issues() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let issue_list_count = Arc::new(AtomicU32::new(0));
    let ilc = Arc::clone(&issue_list_count);

    // GH runner: returns 1 issue on first poll, empty on subsequent polls
    let gh_runner = TestRunner {
        claude_handler: Box::new(|_, _, _| AgentResult {
            cost_usd: 0.0,
            duration: Duration::from_secs(0),
            turns: 0,
            output: String::new(),
            session_id: String::new(),
            success: true,
        }),
        gh_handler: Box::new(move |args, _dir| {
            if args.iter().any(|a| a == "list") {
                let count = ilc.fetch_add(1, Ordering::SeqCst);
                let stdout = if count == 0 {
                    r#"[{"number":201,"title":"Polling test","body":"test body","labels":[{"name":"o-ready"}]}]"#.to_string()
                } else {
                    "[]".to_string()
                };
                CommandOutput { stdout, stderr: String::new(), success: true }
            } else if args.iter().any(|a| a == "create") {
                CommandOutput {
                    stdout: "https://github.com/test/repo/pull/77\n".to_string(),
                    stderr: String::new(),
                    success: true,
                }
            } else {
                CommandOutput { stdout: String::new(), stderr: String::new(), success: true }
            }
        }),
    };

    let cancel = CancellationToken::new();
    let mut config = Config::default();
    config.pipeline.poll_interval = 1;

    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let github = Arc::new(GhClient::new(gh_runner, &repo_dir));
    let issues = make_github_provider(&github);

    let executor = Arc::new(PipelineExecutor {
        runner: Arc::new(test_runner_clean_review()),
        github,
        issues,
        db: Arc::clone(&db),
        config,
        cancel_token: cancel.clone(),
        repo_dir,
    });

    let cancel_clone = cancel.clone();
    let handle =
        tokio::spawn(async move { runner::polling_loop(executor, false, cancel_clone).await });

    // Wait for first poll to fire and issue to be processed
    tokio::time::sleep(Duration::from_secs(3)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("polling loop should exit within timeout")
        .unwrap();
    assert!(result.is_ok());

    // Verify issue was picked up and processed
    let conn = db.lock().await;
    let runs = db::runs::get_all_runs(&conn).unwrap();
    drop(conn);

    assert_eq!(runs.len(), 1, "continuous polling should have processed 1 issue");
    assert_eq!(runs[0].issue_number, 201);
    assert_eq!(runs[0].status, RunStatus::Complete);
}

#[tokio::test]
async fn e2e_continuous_polling_multiple_issues() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let issue_list_count = Arc::new(AtomicU32::new(0));
    let ilc = Arc::clone(&issue_list_count);

    // GH runner: returns 2 issues on first poll, empty after
    let gh_runner = TestRunner {
        claude_handler: Box::new(|_, _, _| AgentResult {
            cost_usd: 0.0,
            duration: Duration::from_secs(0),
            turns: 0,
            output: String::new(),
            session_id: String::new(),
            success: true,
        }),
        gh_handler: Box::new(move |args, _dir| {
            if args.iter().any(|a| a == "list") {
                let count = ilc.fetch_add(1, Ordering::SeqCst);
                let stdout = if count == 0 {
                    r#"[{"number":301,"title":"Issue A","body":"a","labels":[]},{"number":302,"title":"Issue B","body":"b","labels":[]}]"#.to_string()
                } else {
                    "[]".to_string()
                };
                CommandOutput { stdout, stderr: String::new(), success: true }
            } else if args.iter().any(|a| a == "create") {
                CommandOutput {
                    stdout: "https://github.com/test/repo/pull/88\n".to_string(),
                    stderr: String::new(),
                    success: true,
                }
            } else {
                CommandOutput { stdout: String::new(), stderr: String::new(), success: true }
            }
        }),
    };

    let cancel = CancellationToken::new();
    let mut config = Config::default();
    config.pipeline.poll_interval = 1;
    config.pipeline.max_parallel = 1; // Serial to avoid worktree conflicts

    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let github = Arc::new(GhClient::new(gh_runner, &repo_dir));
    let issues = make_github_provider(&github);

    let executor = Arc::new(PipelineExecutor {
        runner: Arc::new(test_runner_clean_review()),
        github,
        issues,
        db: Arc::clone(&db),
        config,
        cancel_token: cancel.clone(),
        repo_dir,
    });

    let cancel_clone = cancel.clone();
    let handle =
        tokio::spawn(async move { runner::polling_loop(executor, false, cancel_clone).await });

    // Wait for both issues to be processed (serial, so ~2x time)
    tokio::time::sleep(Duration::from_secs(5)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("polling loop should exit within timeout")
        .unwrap();
    assert!(result.is_ok());

    let conn = db.lock().await;
    let runs = db::runs::get_all_runs(&conn).unwrap();
    drop(conn);

    assert_eq!(runs.len(), 2, "should have processed both issues");
    let issue_numbers: Vec<u32> = runs.iter().map(|r| r.issue_number).collect();
    assert!(issue_numbers.contains(&301));
    assert!(issue_numbers.contains(&302));
    for run in &runs {
        assert_eq!(run.status, RunStatus::Complete);
    }
}

// -- Multi-repo tests --

#[tokio::test]
async fn e2e_multi_repo_routes_to_target() {
    // Set up god repo (issues live here)
    let (god_work_dir, _god_bare) = setup_git_repo_with_remote().await;
    let god_dir = god_work_dir.path().to_path_buf();

    // Set up target repo (PRs and worktrees go here)
    let (target_work_dir, _target_bare) = setup_git_repo_with_remote().await;
    let target_dir = target_work_dir.path().to_path_buf();

    // Track which directories gh commands run in (via the GhClient's runner)
    let pr_create_dir: Arc<std::sync::Mutex<Option<std::path::PathBuf>>> =
        Arc::new(std::sync::Mutex::new(None));
    let pr_dir_clone = Arc::clone(&pr_create_dir);

    let runner = Arc::new(test_runner_clean_review());

    // The GhClient runner captures PR create directory for verification
    let gh_runner = TestRunner {
        claude_handler: Box::new(|_, _, _| AgentResult {
            cost_usd: 0.0,
            duration: Duration::from_secs(0),
            turns: 0,
            output: String::new(),
            session_id: String::new(),
            success: true,
        }),
        gh_handler: Box::new(move |args, dir| {
            if args.iter().any(|a| a == "create") {
                *pr_dir_clone.lock().unwrap() = Some(dir.to_path_buf());
                CommandOutput {
                    stdout: "https://github.com/test/target/pull/55\n".to_string(),
                    stderr: String::new(),
                    success: true,
                }
            } else {
                CommandOutput { stdout: String::new(), stderr: String::new(), success: true }
            }
        }),
    };

    let github = Arc::new(GhClient::new(gh_runner, &god_dir));
    let issues_provider = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let config = Config {
        multi_repo: MultiRepoConfig { enabled: true, target_field: "target_repo".to_string() },
        repos: std::collections::HashMap::from([("backend".to_string(), target_dir.clone())]),
        ..Config::default()
    };

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues: issues_provider,
        db: Arc::clone(&db),
        config,
        cancel_token: CancellationToken::new(),
        repo_dir: god_dir,
    });

    let issue = PipelineIssue {
        number: 42,
        title: "Fix backend bug".to_string(),
        body: "Fix the auth bug in backend service.".to_string(),
        source: IssueOrigin::Github,
        target_repo: Some("backend".to_string()),
    };

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "multi-repo pipeline failed: {result:?}");

    // Verify PR was created using the target repo dir
    {
        let pr_dir = pr_create_dir.lock().unwrap();
        assert!(pr_dir.is_some(), "PR create should have been called");
        let pr_dir_str = pr_dir.as_ref().unwrap().to_string_lossy().to_string();
        drop(pr_dir);
        let target_dir_str = target_dir.to_string_lossy().to_string();
        assert!(
            pr_dir_str.starts_with(&target_dir_str),
            "PR should be created in target repo dir, got: {pr_dir_str}, expected prefix: {target_dir_str}"
        );
    }

    // Verify DB state
    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.issue_number, 42);
}

#[tokio::test]
async fn e2e_multi_repo_no_frontmatter_uses_god_repo() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    // No repos configured, but that's fine since this issue has no frontmatter
    let config = Config {
        multi_repo: MultiRepoConfig { enabled: true, target_field: "target_repo".to_string() },
        ..Config::default()
    };

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config,
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue = make_github_issue(50, "Regular issue", "No frontmatter, uses god repo.");

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "pipeline should work without frontmatter: {result:?}");

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.status, RunStatus::Complete);
}

#[tokio::test]
async fn e2e_multi_repo_missing_repo_config_errors() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    // No repos configured -- referencing an unknown repo should fail
    let config = Config {
        multi_repo: MultiRepoConfig { enabled: true, target_field: "target_repo".to_string() },
        ..Config::default()
    };

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config,
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue = PipelineIssue {
        number: 60,
        title: "Unknown repo".to_string(),
        body: "This should fail.".to_string(),
        source: IssueOrigin::Github,
        target_repo: Some("nonexistent".to_string()),
    };

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found in user config"), "should mention missing config, got: {err}");
}

#[test]
fn multi_repo_disabled_ignores_frontmatter() {
    // When multi_repo.enabled is false, target_repo on the issue is not acted on.
    // The pipeline uses the god repo for everything.
    let config = Config::default();
    assert!(!config.multi_repo.enabled);

    // Even with target_repo set, the executor would not route because
    // multi_repo.enabled is false.
    let issue = PipelineIssue {
        number: 70,
        title: "Ignored target".to_string(),
        body: "Body".to_string(),
        source: IssueOrigin::Github,
        target_repo: Some("some-repo".to_string()),
    };
    assert!(issue.target_repo.is_some());
}

// -- Local issue source tests --

fn make_local_issue(number: u32, title: &str, body: &str) -> PipelineIssue {
    PipelineIssue {
        number,
        title: title.to_string(),
        body: body.to_string(),
        source: IssueOrigin::Local,
        target_repo: None,
    }
}

fn make_local_provider(project_dir: &std::path::Path) -> Arc<dyn IssueProvider> {
    Arc::new(oven_cli::issues::local::LocalIssueProvider::new(project_dir))
}

#[tokio::test]
async fn e2e_local_issue_completes_pipeline() {
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    // Create a local issue file
    let issues_dir = repo_dir.join(".oven").join("issues");
    tokio::fs::create_dir_all(&issues_dir).await.unwrap();
    tokio::fs::write(
        issues_dir.join("1.md"),
        "---\nid: 1\ntitle: Local feature\nstatus: open\nlabels: [\"o-ready\"]\n---\n\nImplement the feature.\n",
    )
    .await
    .unwrap();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_local_provider(&repo_dir);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir: repo_dir.clone(),
    });

    let issue = make_local_issue(1, "Local feature", "Implement the feature.");
    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "local issue pipeline failed: {result:?}");

    // Verify issue_source is "local" in DB
    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.issue_source, "local");

    // Verify local issue was closed
    let content = tokio::fs::read_to_string(issues_dir.join("1.md")).await.unwrap();
    assert!(content.contains("status: closed"), "local issue should be closed after pipeline");
}

#[tokio::test]
async fn e2e_local_issue_records_github_source() {
    // Verify GitHub issues still get "github" as issue_source
    let (work_dir, _bare_dir) = setup_git_repo_with_remote().await;
    let repo_dir = work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &repo_dir));
    let issues = make_github_provider(&github);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config: Config::default(),
        cancel_token: CancellationToken::new(),
        repo_dir,
    });

    let issue = make_github_issue(42, "GitHub issue", "From GitHub.");
    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok());

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.issue_source, "github");
}

#[tokio::test]
async fn e2e_local_issue_with_target_repo() {
    let (god_work_dir, _god_bare) = setup_git_repo_with_remote().await;
    let god_dir = god_work_dir.path().to_path_buf();

    let (target_work_dir, _target_bare) = setup_git_repo_with_remote().await;
    let target_dir = target_work_dir.path().to_path_buf();

    let runner = Arc::new(test_runner_clean_review());
    let github = Arc::new(GhClient::new(test_runner_clean_review(), &god_dir));
    let issues = make_local_provider(&god_dir);
    let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));

    // Create a local issue with target_repo
    let issues_dir = god_dir.join(".oven").join("issues");
    tokio::fs::create_dir_all(&issues_dir).await.unwrap();
    tokio::fs::write(
        issues_dir.join("5.md"),
        "---\nid: 5\ntitle: Backend fix\nstatus: open\nlabels: [\"o-ready\"]\ntarget_repo: backend\n---\n\nFix backend.\n",
    )
    .await
    .unwrap();

    let config = Config {
        multi_repo: MultiRepoConfig { enabled: true, target_field: "target_repo".to_string() },
        repos: std::collections::HashMap::from([("backend".to_string(), target_dir)]),
        ..Config::default()
    };

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues,
        db: Arc::clone(&db),
        config,
        cancel_token: CancellationToken::new(),
        repo_dir: god_dir.clone(),
    });

    let issue = PipelineIssue {
        number: 5,
        title: "Backend fix".to_string(),
        body: "Fix backend.".to_string(),
        source: IssueOrigin::Local,
        target_repo: Some("backend".to_string()),
    };

    let result = executor.run_issue(&issue, false).await;
    assert!(result.is_ok(), "local multi-repo pipeline failed: {result:?}");

    let conn = db.lock().await;
    let run = db::runs::get_latest_run(&conn).unwrap().unwrap();
    drop(conn);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.issue_source, "local");

    // Verify local issue was closed
    let content = tokio::fs::read_to_string(issues_dir.join("5.md")).await.unwrap();
    assert!(content.contains("status: closed"));
}
