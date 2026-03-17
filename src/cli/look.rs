use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use super::{GlobalOpts, LookArgs};
use crate::db::{self, AgentRun, ReviewFinding, Run};

pub async fn run(args: LookArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;

    if args.stream {
        return show_stream(&project_dir, args.agent.as_deref());
    }

    let logs_root = project_dir.join(".oven").join("logs");

    let log_dir = if let Some(ref run_id) = args.run_id {
        let dir = logs_root.join(run_id);
        if !dir.exists() {
            anyhow::bail!("no log directory found for run {run_id}");
        }
        dir
    } else {
        find_latest_log_dir(&logs_root)?.context("no log directories found in .oven/logs/")?
    };

    let log_file = log_dir.join("pipeline.log");
    if !log_file.exists() {
        anyhow::bail!("no pipeline.log found in {}", log_dir.display());
    }

    let is_active = is_oven_running(&project_dir);

    if is_active {
        tail_log(&log_file, args.agent.as_deref()).await?;
    } else {
        dump_log(&log_file, args.agent.as_deref()).await?;
    }

    Ok(())
}

/// Query the database and display agent progress for active (or recent) runs.
fn show_stream(project_dir: &Path, agent_filter: Option<&str>) -> Result<()> {
    let db_path = project_dir.join(".oven").join("oven.db");
    if !db_path.exists() {
        anyhow::bail!("no database found at {}", db_path.display());
    }
    let conn = db::open(&db_path)?;

    let mut runs = db::runs::get_active_runs(&conn)?;
    if runs.is_empty() {
        // Fall back to the most recent run
        if let Some(latest) = db::runs::get_latest_run(&conn)? {
            runs.push(latest);
        } else {
            println!("no runs found");
            return Ok(());
        }
    }

    for run in &runs {
        let agents = db::agent_runs::get_agent_runs_for_run(&conn, &run.id)?;
        let findings = collect_run_findings(&conn, &agents)?;
        print_run_status(run, &agents, &findings, agent_filter);
    }

    Ok(())
}

/// Collect unresolved findings across all reviewer agent runs for a pipeline run.
fn collect_run_findings(
    conn: &rusqlite::Connection,
    agents: &[AgentRun],
) -> Result<Vec<ReviewFinding>> {
    let mut findings = Vec::new();
    for ar in agents {
        if ar.agent == "reviewer" {
            let mut f = db::agent_runs::get_findings_for_agent_run(conn, ar.id)?;
            findings.append(&mut f);
        }
    }
    Ok(findings)
}

fn print_run_status(
    run: &Run,
    agents: &[AgentRun],
    findings: &[ReviewFinding],
    agent_filter: Option<&str>,
) {
    let branch = run.branch.as_deref().unwrap_or("--");
    let pr = run.pr_number.map_or_else(|| "--".to_string(), |n| format!("#{n}"));
    println!(
        "issue #{:<6} {} {:>14}  ${:.2}  {}",
        run.issue_number, pr, run.status, run.cost_usd, branch,
    );

    for ar in agents {
        if let Some(filter) = agent_filter {
            if ar.agent != filter {
                continue;
            }
        }
        let status_icon = match ar.status.as_str() {
            "complete" => "done",
            "running" => "...",
            "failed" => "FAIL",
            _ => &ar.status,
        };
        let summary =
            ar.output_summary.as_deref().map(|s| truncate_line(s, 80)).unwrap_or_default();
        println!(
            "  {:<14} cycle {:<2} {:<6} {:>3} turns  ${:.2}  {}",
            ar.agent, ar.cycle, status_icon, ar.turns, ar.cost_usd, summary,
        );
    }

    let unresolved: Vec<_> = findings.iter().filter(|f| !f.resolved).collect();
    if !unresolved.is_empty() {
        let mut buf = String::new();
        let _ = writeln!(buf, "  findings ({} unresolved):", unresolved.len());
        for f in &unresolved {
            let loc = match (&f.file_path, f.line_number) {
                (Some(path), Some(line)) => format!(" {path}:{line}"),
                (Some(path), None) => format!(" {path}"),
                _ => String::new(),
            };
            let _ = writeln!(buf, "    {}/{}{} -- {}", f.severity, f.category, loc, f.message);
        }
        print!("{buf}");
    }

    println!();
}

/// Truncate a string to a single line of at most `max` chars.
fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.len() <= max {
        line.to_string()
    } else {
        format!("{}...", &line[..max.saturating_sub(3)])
    }
}

/// Find the most recently modified log directory in `.oven/logs/`.
fn find_latest_log_dir(logs_root: &Path) -> Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(logs_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading log directory"),
    };

    let mut dirs: Vec<_> = entries
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().ok().is_some_and(|t| t.is_dir()))
        .collect();

    dirs.sort_by(|a, b| {
        let ma = a.metadata().ok().and_then(|m| m.modified().ok());
        let mb = b.metadata().ok().and_then(|m| m.modified().ok());
        mb.cmp(&ma)
    });

    Ok(dirs.first().map(std::fs::DirEntry::path))
}

/// Check whether an oven process is currently running via PID file.
fn is_oven_running(project_dir: &Path) -> bool {
    let pid_path = project_dir.join(".oven").join("oven.pid");
    let Ok(contents) = std::fs::read_to_string(&pid_path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

async fn dump_log(path: &Path, agent_filter: Option<&str>) -> Result<()> {
    let file = tokio::fs::File::open(path).await.context("reading log file")?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await.context("reading log line")? {
        if let Some(formatted) = format_log_line(&line, agent_filter) {
            println!("{formatted}");
        }
    }

    Ok(())
}

async fn tail_log(path: &Path, agent_filter: Option<&str>) -> Result<()> {
    let cancel = CancellationToken::new();
    let cancel_for_signal = cancel.clone();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_for_signal.cancel();
        }
    });

    let file = tokio::fs::File::open(path).await.context("opening log file")?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if let Some(formatted) = format_log_line(trimmed, agent_filter) {
                            println!("{formatted}");
                        }
                        line.clear();
                    }
                    Err(e) => return Err(e).context("reading log file"),
                }
            }
        }
    }

    Ok(())
}

/// Messages to suppress in formatted output (noisy polling lines).
const SUPPRESSED_MESSAGES: &[&str] = &["no actionable issues, waiting"];

/// Format a JSON tracing log line into a compact human-readable string.
///
/// Returns `None` if the line should be suppressed (noisy polling messages)
/// or doesn't match the agent filter.
fn format_log_line(line: &str, agent_filter: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    let fields = v.get("fields")?;
    let message = fields.get("message").and_then(|m| m.as_str()).unwrap_or("");

    // Suppress noisy polling messages
    if SUPPRESSED_MESSAGES.contains(&message) {
        return None;
    }

    // Agent filter
    if let Some(filter) = agent_filter {
        let agent = fields.get("agent").and_then(|a| a.as_str()).unwrap_or("");
        if !agent.is_empty() && agent != filter {
            return None;
        }
    }

    // Extract timestamp -- just HH:MM:SS from the ISO string
    let timestamp = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| t.find('T').map(|i| &t[i + 1..]))
        .map_or("??:??:??", |t| if t.len() > 8 { &t[..8] } else { t });

    let level = v.get("level").and_then(|l| l.as_str()).unwrap_or("INFO");

    let level_tag = match level {
        "ERROR" => "ERR ",
        "WARN" => "WARN",
        "DEBUG" => "DBG ",
        _ => "    ",
    };

    // Collect extra fields (skip "message" since we already have it)
    let mut extras = Vec::new();
    if let Some(obj) = fields.as_object() {
        for (k, val) in obj {
            if k == "message" {
                continue;
            }
            let val_str = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            extras.push(format!("{k}={val_str}"));
        }
    }

    let extra_str =
        if extras.is_empty() { String::new() } else { format!("  {}", extras.join(" ")) };

    Some(format!("{timestamp} {level_tag} {message}{extra_str}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RunStatus;

    fn json_log(msg: &str, agent: Option<&str>) -> String {
        let agent_field = agent.map_or_else(String::new, |a| format!(r#","agent":"{a}""#));
        format!(
            r#"{{"timestamp":"2026-03-17T09:53:34.350926Z","level":"INFO","fields":{{"message":"{msg}"{agent_field}}},"target":"oven_cli::test"}}"#
        )
    }

    #[test]
    fn format_log_line_basic() {
        let line = json_log("agent starting", Some("reviewer"));
        let formatted = format_log_line(&line, None).unwrap();
        assert!(formatted.contains("09:53:34"));
        assert!(formatted.contains("agent starting"));
        assert!(formatted.contains("agent=reviewer"));
        // Should NOT contain full module path
        assert!(!formatted.contains("oven_cli"));
    }

    #[test]
    fn format_log_line_suppresses_waiting() {
        let line = json_log("no actionable issues, waiting", None);
        assert!(format_log_line(&line, None).is_none());
    }

    #[test]
    fn format_log_line_agent_filter() {
        let line = json_log("starting", Some("implementer"));
        // Filter for reviewer should exclude implementer
        assert!(format_log_line(&line, Some("reviewer")).is_none());
        // Filter for implementer should include
        assert!(format_log_line(&line, Some("implementer")).is_some());
    }

    #[test]
    fn format_log_line_no_agent_passes_filter() {
        // Lines without an agent field should always pass the agent filter
        let line = json_log("pipeline started", None);
        assert!(format_log_line(&line, Some("reviewer")).is_some());
    }

    #[test]
    fn format_log_line_non_json_returns_none() {
        assert!(format_log_line("not json at all", None).is_none());
    }

    #[test]
    fn format_log_line_error_level() {
        let line = r#"{"timestamp":"2026-03-17T10:00:00Z","level":"ERROR","fields":{"message":"pipeline failed"},"target":"test"}"#;
        let formatted = format_log_line(line, None).unwrap();
        assert!(formatted.contains("ERR"));
    }

    #[test]
    fn find_latest_log_dir_missing_root_returns_none() {
        let result = find_latest_log_dir(Path::new("/nonexistent/path/logs")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_latest_log_dir_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = find_latest_log_dir(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_latest_log_dir_picks_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("aaaa1111");
        let dir_b = tmp.path().join("bbbb2222");
        std::fs::create_dir(&dir_a).unwrap();
        // Small sleep to ensure different mtimes
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::create_dir(&dir_b).unwrap();

        let result = find_latest_log_dir(tmp.path()).unwrap().unwrap();
        assert_eq!(result.file_name().unwrap(), "bbbb2222");
    }

    #[test]
    fn is_oven_running_returns_false_when_no_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_oven_running(tmp.path()));
    }

    #[test]
    fn is_oven_running_returns_false_for_stale_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let oven_dir = tmp.path().join(".oven");
        std::fs::create_dir_all(&oven_dir).unwrap();
        // PID 99999999 almost certainly doesn't exist
        std::fs::write(oven_dir.join("oven.pid"), "99999999").unwrap();
        assert!(!is_oven_running(tmp.path()));
    }

    #[test]
    fn truncate_line_short() {
        assert_eq!(truncate_line("hello", 10), "hello");
    }

    #[test]
    fn truncate_line_long() {
        let long = "a".repeat(100);
        let result = truncate_line(&long, 20);
        assert_eq!(result.len(), 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_line_multiline_uses_first() {
        assert_eq!(truncate_line("first\nsecond\nthird", 80), "first");
    }

    #[test]
    fn print_run_status_formats_correctly() {
        let run = Run {
            id: "abc12345".to_string(),
            issue_number: 42,
            status: RunStatus::Reviewing,
            pr_number: Some(10),
            branch: Some("oven/issue-42".to_string()),
            worktree_path: None,
            cost_usd: 2.34,
            auto_merge: false,
            started_at: "2026-03-15T10:00:00".to_string(),
            finished_at: None,
            error_message: None,
            complexity: "full".to_string(),
            issue_source: "github".to_string(),
        };
        let agents = vec![
            AgentRun {
                id: 1,
                run_id: "abc12345".to_string(),
                agent: "implementer".to_string(),
                cycle: 1,
                status: "complete".to_string(),
                cost_usd: 1.50,
                turns: 12,
                started_at: "2026-03-15T10:00:00".to_string(),
                finished_at: Some("2026-03-15T10:05:00".to_string()),
                output_summary: Some("Added auth flow".to_string()),
                error_message: None,
                raw_output: None,
            },
            AgentRun {
                id: 2,
                run_id: "abc12345".to_string(),
                agent: "reviewer".to_string(),
                cycle: 1,
                status: "running".to_string(),
                cost_usd: 0.84,
                turns: 5,
                started_at: "2026-03-15T10:05:00".to_string(),
                finished_at: None,
                output_summary: None,
                error_message: None,
                raw_output: None,
            },
        ];
        // Smoke test: should not panic
        print_run_status(&run, &agents, &[], None);
    }

    #[test]
    fn print_run_status_with_agent_filter() {
        let run = Run {
            id: "abc12345".to_string(),
            issue_number: 42,
            status: RunStatus::Reviewing,
            pr_number: Some(10),
            branch: Some("oven/issue-42".to_string()),
            worktree_path: None,
            cost_usd: 2.34,
            auto_merge: false,
            started_at: "2026-03-15T10:00:00".to_string(),
            finished_at: None,
            error_message: None,
            complexity: "full".to_string(),
            issue_source: "github".to_string(),
        };
        let agents = vec![AgentRun {
            id: 1,
            run_id: "abc12345".to_string(),
            agent: "implementer".to_string(),
            cycle: 1,
            status: "complete".to_string(),
            cost_usd: 1.50,
            turns: 12,
            started_at: "2026-03-15T10:00:00".to_string(),
            finished_at: Some("2026-03-15T10:05:00".to_string()),
            output_summary: Some("ok".to_string()),
            error_message: None,
            raw_output: None,
        }];
        // Filter to reviewer (which doesn't exist) -- should not panic
        print_run_status(&run, &agents, &[], Some("reviewer"));
    }

    #[test]
    fn show_stream_no_database() {
        let tmp = tempfile::tempdir().unwrap();
        let result = show_stream(tmp.path(), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no database"));
    }

    #[test]
    fn show_stream_empty_database() {
        let tmp = tempfile::tempdir().unwrap();
        let oven_dir = tmp.path().join(".oven");
        std::fs::create_dir_all(&oven_dir).unwrap();
        let db_path = oven_dir.join("oven.db");
        // Open and immediately close to create the DB with migrations applied
        drop(db::open(&db_path).unwrap());

        // Should print "no runs found" and succeed
        show_stream(tmp.path(), None).unwrap();
    }
}
