pub mod agent_runs;
pub mod runs;

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

pub static MIGRATIONS: std::sync::LazyLock<Migrations<'static>> = std::sync::LazyLock::new(|| {
    Migrations::new(vec![
        M::up(
            "CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    issue_number INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    pr_number INTEGER,
    branch TEXT,
    worktree_path TEXT,
    cost_usd REAL NOT NULL DEFAULT 0.0,
    auto_merge INTEGER NOT NULL DEFAULT 0,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT,
    error_message TEXT
);

CREATE TABLE agent_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    agent TEXT NOT NULL,
    cycle INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'pending',
    cost_usd REAL NOT NULL DEFAULT 0.0,
    turns INTEGER NOT NULL DEFAULT 0,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT,
    output_summary TEXT,
    error_message TEXT
);

CREATE TABLE review_findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_run_id INTEGER NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
    severity TEXT NOT NULL CHECK (severity IN ('critical', 'warning', 'info')),
    category TEXT NOT NULL,
    file_path TEXT,
    line_number INTEGER,
    message TEXT NOT NULL,
    resolved INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_runs_status ON runs(status);
CREATE INDEX idx_runs_issue ON runs(issue_number);
CREATE INDEX idx_agent_runs_run ON agent_runs(run_id);
CREATE INDEX idx_findings_agent_run ON review_findings(agent_run_id);
CREATE INDEX idx_findings_severity ON review_findings(severity);",
        ),
        M::up("ALTER TABLE runs ADD COLUMN complexity TEXT NOT NULL DEFAULT 'full';"),
    ])
});

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let mut conn = Connection::open(path).context("opening database")?;
    configure(&conn)?;
    MIGRATIONS.to_latest(&mut conn).context("running database migrations")?;
    Ok(conn)
}

pub fn open_in_memory() -> anyhow::Result<Connection> {
    let mut conn = Connection::open_in_memory().context("opening in-memory database")?;
    configure(&conn)?;
    MIGRATIONS.to_latest(&mut conn).context("running database migrations")?;
    Ok(conn)
}

fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Run status for pipeline runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunStatus {
    Pending,
    Implementing,
    Reviewing,
    Fixing,
    Merging,
    Complete,
    Failed,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Implementing => "implementing",
            Self::Reviewing => "reviewing",
            Self::Fixing => "fixing",
            Self::Merging => "merging",
            Self::Complete => "complete",
            Self::Failed => "failed",
        })
    }
}

impl std::str::FromStr for RunStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "implementing" => Ok(Self::Implementing),
            "reviewing" => Ok(Self::Reviewing),
            "fixing" => Ok(Self::Fixing),
            "merging" => Ok(Self::Merging),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown run status: {other}"),
        }
    }
}

/// A pipeline run record.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub issue_number: u32,
    pub status: RunStatus,
    pub pr_number: Option<u32>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub cost_usd: f64,
    pub auto_merge: bool,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error_message: Option<String>,
    pub complexity: String,
}

/// An agent execution record.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub id: i64,
    pub run_id: String,
    pub agent: String,
    pub cycle: u32,
    pub status: String,
    pub cost_usd: f64,
    pub turns: u32,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub output_summary: Option<String>,
    pub error_message: Option<String>,
}

/// A review finding from the reviewer agent.
#[derive(Debug, Clone)]
pub struct ReviewFinding {
    pub id: i64,
    pub agent_run_id: i64,
    pub severity: String,
    pub category: String,
    pub file_path: Option<String>,
    pub line_number: Option<u32>,
    pub message: String,
    pub resolved: bool,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const ALL_STATUSES: [RunStatus; 7] = [
        RunStatus::Pending,
        RunStatus::Implementing,
        RunStatus::Reviewing,
        RunStatus::Fixing,
        RunStatus::Merging,
        RunStatus::Complete,
        RunStatus::Failed,
    ];

    proptest! {
        #[test]
        fn run_status_display_fromstr_roundtrip(idx in 0..7usize) {
            let status = ALL_STATUSES[idx];
            let s = status.to_string();
            let parsed: RunStatus = s.parse().unwrap();
            assert_eq!(status, parsed);
        }

        #[test]
        fn arbitrary_strings_never_panic_on_parse(s in "\\PC{1,50}") {
            // Parsing arbitrary strings should never panic, only return Ok or Err
            let _ = s.parse::<RunStatus>();
        }
    }

    #[test]
    fn migrations_validate() {
        MIGRATIONS.validate().unwrap();
    }

    #[test]
    fn open_in_memory_succeeds() {
        let conn = open_in_memory().unwrap();
        // Verify tables exist by querying them
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn run_status_display_roundtrip() {
        let statuses = [
            RunStatus::Pending,
            RunStatus::Implementing,
            RunStatus::Reviewing,
            RunStatus::Fixing,
            RunStatus::Merging,
            RunStatus::Complete,
            RunStatus::Failed,
        ];
        for status in statuses {
            let s = status.to_string();
            let parsed: RunStatus = s.parse().unwrap();
            assert_eq!(status, parsed);
        }
    }

    #[test]
    fn run_status_unknown_returns_error() {
        let result = "banana".parse::<RunStatus>();
        assert!(result.is_err());
    }
}
