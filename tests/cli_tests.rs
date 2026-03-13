mod common;

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

#[test]
fn help_shows_all_commands() {
    Command::cargo_bin("oven")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("prep"))
        .stdout(predicate::str::contains("on"))
        .stdout(predicate::str::contains("off"))
        .stdout(predicate::str::contains("look"))
        .stdout(predicate::str::contains("report"))
        .stdout(predicate::str::contains("clean"))
        .stdout(predicate::str::contains("ticket"));
}

#[test]
fn version_flag_works() {
    Command::cargo_bin("oven")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("oven"));
}

#[test]
fn prep_creates_project_structure() {
    let dir = assert_fs::TempDir::new().unwrap();

    // git init
    std::process::Command::new("git").args(["init"]).current_dir(dir.path()).output().unwrap();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("prep")
        .assert()
        .success()
        .stdout(predicate::str::contains("project ready"));

    dir.child("recipe.toml").assert(predicate::path::exists());
    dir.child(".oven").assert(predicate::path::is_dir());
    dir.child(".oven/oven.db").assert(predicate::path::exists());
    dir.child(".oven/logs").assert(predicate::path::is_dir());
    dir.child(".oven/worktrees").assert(predicate::path::is_dir());
    dir.child(".oven/issues").assert(predicate::path::is_dir());
    dir.child(".claude/agents/implementer.md").assert(predicate::path::exists());
    dir.child(".claude/agents/reviewer.md").assert(predicate::path::exists());
}

#[test]
fn prep_skips_existing_files() {
    let dir = assert_fs::TempDir::new().unwrap();
    std::process::Command::new("git").args(["init"]).current_dir(dir.path()).output().unwrap();

    // First run
    Command::cargo_bin("oven").unwrap().current_dir(dir.path()).arg("prep").assert().success();

    // Second run should say "exists, skipped"
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("prep")
        .assert()
        .success()
        .stdout(predicate::str::contains("exists, skipped"));
}

#[test]
fn prep_force_overwrites() {
    let dir = assert_fs::TempDir::new().unwrap();
    std::process::Command::new("git").args(["init"]).current_dir(dir.path()).output().unwrap();

    // First run
    Command::cargo_bin("oven").unwrap().current_dir(dir.path()).arg("prep").assert().success();

    // Force run should say "overwritten"
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["prep", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("overwritten"));
}

#[test]
fn ticket_lifecycle() {
    let dir = common::setup_oven_project();

    // Create
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "create", "Test issue", "--body", "body text", "--ready"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created ticket #1"));

    // List
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Test issue"))
        .stdout(predicate::str::contains("o-ready"));

    // View
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "view", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("title: Test issue"))
        .stdout(predicate::str::contains("body text"));

    // Close
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "close", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("closed ticket #1"));

    // Verify closed
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "view", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("status: closed"));
}

#[test]
fn ticket_list_empty() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no tickets found"));
}

#[test]
fn ticket_view_nonexistent() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "view", "999"])
        .assert()
        .failure();
}

#[test]
fn off_without_pid_file_errors() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("off")
        .assert()
        .failure()
        .stderr(predicate::str::contains("oven.pid"));
}

#[test]
fn report_no_runs() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("report")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no runs found"));
}

#[test]
fn report_all_no_runs() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["report", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no runs found"));
}

#[test]
fn clean_in_prepped_project() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("clean")
        .assert()
        .success()
        .stdout(predicate::str::contains("pruned"))
        .stdout(predicate::str::contains("deleted"));
}

#[test]
fn look_no_runs() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("look")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no runs found"));
}

#[test]
fn ticket_create_multiple_auto_increments() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "create", "First issue"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created ticket #1"));

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "create", "Second issue"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created ticket #2"));
}

#[test]
fn report_with_seeded_run() {
    let dir = common::setup_oven_project();
    let db_path = dir.path().join(".oven").join("oven.db");
    let conn = oven_cli::db::open(&db_path).unwrap();

    oven_cli::db::runs::insert_run(
        &conn,
        &oven_cli::db::Run {
            id: "abc12345".to_string(),
            issue_number: 42,
            status: oven_cli::db::RunStatus::Complete,
            pr_number: Some(99),
            branch: Some("oven/issue-42-abc".to_string()),
            worktree_path: None,
            cost_usd: 4.23,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:08:32".to_string()),
            error_message: None,
        },
    )
    .unwrap();
    drop(conn);

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .arg("report")
        .assert()
        .success()
        .stdout(predicate::str::contains("abc12345"))
        .stdout(predicate::str::contains("#42"))
        .stdout(predicate::str::contains("$4.23"));
}

#[test]
fn report_json_with_seeded_run() {
    let dir = common::setup_oven_project();
    let db_path = dir.path().join(".oven").join("oven.db");
    let conn = oven_cli::db::open(&db_path).unwrap();

    oven_cli::db::runs::insert_run(
        &conn,
        &oven_cli::db::Run {
            id: "json1234".to_string(),
            issue_number: 7,
            status: oven_cli::db::RunStatus::Complete,
            pr_number: None,
            branch: None,
            worktree_path: None,
            cost_usd: 1.50,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:05:00".to_string()),
            error_message: None,
        },
    )
    .unwrap();
    drop(conn);

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["report", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"id\": \"json1234\""))
        .stdout(predicate::str::contains("\"issue_number\": 7"));
}

#[test]
fn report_all_with_multiple_runs() {
    let dir = common::setup_oven_project();
    let db_path = dir.path().join(".oven").join("oven.db");
    let conn = oven_cli::db::open(&db_path).unwrap();

    for i in 1..=3 {
        oven_cli::db::runs::insert_run(
            &conn,
            &oven_cli::db::Run {
                id: format!("aabb00{i}0"),
                issue_number: i,
                status: oven_cli::db::RunStatus::Complete,
                pr_number: None,
                branch: None,
                worktree_path: None,
                cost_usd: f64::from(i),
                auto_merge: false,
                started_at: format!("2026-03-{i:02}T10:00:00"),
                finished_at: None,
                error_message: None,
            },
        )
        .unwrap();
    }
    drop(conn);

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["report", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("aabb0010"))
        .stdout(predicate::str::contains("aabb0020"))
        .stdout(predicate::str::contains("aabb0030"));
}

#[test]
fn look_with_seeded_run_and_log() {
    let dir = common::setup_oven_project();
    let db_path = dir.path().join(".oven").join("oven.db");
    let conn = oven_cli::db::open(&db_path).unwrap();

    oven_cli::db::runs::insert_run(
        &conn,
        &oven_cli::db::Run {
            id: "look1234".to_string(),
            issue_number: 10,
            status: oven_cli::db::RunStatus::Complete,
            pr_number: None,
            branch: None,
            worktree_path: None,
            cost_usd: 0.0,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:05:00".to_string()),
            error_message: None,
        },
    )
    .unwrap();
    drop(conn);

    // Create log directory and file
    let log_dir = dir.path().join(".oven").join("logs").join("look1234");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(
        log_dir.join("pipeline.log"),
        "agent=implementer cycle=1 starting\nagent=reviewer cycle=1 reviewing\n",
    )
    .unwrap();

    // Dump completed run
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["look", "look1234"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent=implementer"))
        .stdout(predicate::str::contains("agent=reviewer"));
}

#[test]
fn look_with_agent_filter() {
    let dir = common::setup_oven_project();
    let db_path = dir.path().join(".oven").join("oven.db");
    let conn = oven_cli::db::open(&db_path).unwrap();

    oven_cli::db::runs::insert_run(
        &conn,
        &oven_cli::db::Run {
            id: "filt1234".to_string(),
            issue_number: 10,
            status: oven_cli::db::RunStatus::Complete,
            pr_number: None,
            branch: None,
            worktree_path: None,
            cost_usd: 0.0,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:05:00".to_string()),
            error_message: None,
        },
    )
    .unwrap();
    drop(conn);

    let log_dir = dir.path().join(".oven").join("logs").join("filt1234");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(
        log_dir.join("pipeline.log"),
        "agent=implementer starting\nagent=reviewer reviewing\nagent=implementer done\n",
    )
    .unwrap();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["look", "filt1234", "--agent", "reviewer"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent=reviewer"))
        .stdout(predicate::str::contains("agent=implementer").not());
}

#[test]
fn look_missing_log_file_errors() {
    let dir = common::setup_oven_project();
    let db_path = dir.path().join(".oven").join("oven.db");
    let conn = oven_cli::db::open(&db_path).unwrap();

    oven_cli::db::runs::insert_run(
        &conn,
        &oven_cli::db::Run {
            id: "nolog123".to_string(),
            issue_number: 10,
            status: oven_cli::db::RunStatus::Complete,
            pr_number: None,
            branch: None,
            worktree_path: None,
            cost_usd: 0.0,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:05:00".to_string()),
            error_message: None,
        },
    )
    .unwrap();
    drop(conn);

    // Don't create log file
    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["look", "nolog123"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no log file"));
}

#[test]
fn clean_with_selective_flags() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["clean", "--only-logs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed"));

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["clean", "--only-trees"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pruned"));
}

#[test]
fn ticket_list_filters_by_label() {
    let dir = common::setup_oven_project();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "create", "Ready one", "--ready"])
        .assert()
        .success();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "create", "Not ready"])
        .assert()
        .success();

    Command::cargo_bin("oven")
        .unwrap()
        .current_dir(dir.path())
        .args(["ticket", "list", "--label", "o-ready"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Ready one"))
        .stdout(predicate::str::contains("Not ready").not());
}
