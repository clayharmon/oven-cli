#![allow(dead_code)]

use assert_cmd::Command;
use assert_fs::TempDir;

/// Set up a temporary git repository with an initial commit and run `oven prep`.
pub fn setup_oven_project() -> TempDir {
    let dir = TempDir::new().unwrap();

    // Initialize git repo
    std::process::Command::new("git").args(["init"]).current_dir(dir.path()).output().unwrap();

    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    std::fs::write(dir.path().join("README.md"), "# test\n").unwrap();

    std::process::Command::new("git").args(["add", "."]).current_dir(dir.path()).output().unwrap();

    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Run oven prep
    Command::cargo_bin("oven").unwrap().current_dir(dir.path()).arg("prep").assert().success();

    dir
}

/// Create an in-memory test database with migrations applied.
pub fn test_db() -> rusqlite::Connection {
    oven_cli::db::open_in_memory().unwrap()
}
