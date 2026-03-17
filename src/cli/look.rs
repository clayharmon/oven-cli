use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use super::{GlobalOpts, LookArgs};

pub async fn run(args: LookArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
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
    let agent_tag = args.agent.as_deref().map(|a| format!("agent={a}"));

    if is_active {
        tail_log(&log_file, args.agent.as_deref(), agent_tag.as_deref()).await?;
    } else {
        dump_log(&log_file, args.agent.as_deref(), agent_tag.as_deref()).await?;
    }

    Ok(())
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

async fn dump_log(path: &Path, agent_filter: Option<&str>, agent_tag: Option<&str>) -> Result<()> {
    let file = tokio::fs::File::open(path).await.context("reading log file")?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await.context("reading log line")? {
        if should_show_line(&line, agent_filter, agent_tag) {
            println!("{line}");
        }
    }

    Ok(())
}

async fn tail_log(path: &Path, agent_filter: Option<&str>, agent_tag: Option<&str>) -> Result<()> {
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
                        // EOF, wait briefly for more content
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if should_show_line(trimmed, agent_filter, agent_tag) {
                            println!("{trimmed}");
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

fn should_show_line(line: &str, agent_filter: Option<&str>, agent_tag: Option<&str>) -> bool {
    match (agent_filter, agent_tag) {
        (Some(agent), Some(tag)) => line.contains(tag) || line.contains(agent),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_agent_field() {
        let tag = "agent=reviewer";
        assert!(should_show_line(
            r#"{"agent":"reviewer","msg":"ok"}"#,
            Some("reviewer"),
            Some(tag)
        ));
        assert!(!should_show_line(
            r#"{"agent":"implementer","msg":"ok"}"#,
            Some("reviewer"),
            Some(tag)
        ));
    }

    #[test]
    fn no_filter_shows_all() {
        assert!(should_show_line("any line at all", None, None));
    }

    #[test]
    fn filter_matches_substring() {
        assert!(should_show_line(
            "agent=reviewer cycle=1",
            Some("reviewer"),
            Some("agent=reviewer")
        ));
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
}
