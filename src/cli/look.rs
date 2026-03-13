use std::path::Path;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use super::{GlobalOpts, LookArgs};
use crate::db::{self, RunStatus};

pub async fn run(args: LookArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
    let db_path = project_dir.join(".oven").join("oven.db");
    let conn = db::open(&db_path)?;

    let run = if let Some(ref run_id) = args.run_id {
        db::runs::get_run(&conn, run_id)?.with_context(|| format!("run {run_id} not found"))?
    } else {
        db::runs::get_latest_run(&conn)?.context("no runs found")?
    };

    let log_dir = project_dir.join(".oven").join("logs").join(&run.id);
    let log_file = log_dir.join("pipeline.log");

    if !log_file.exists() {
        anyhow::bail!("no log file found for run {}", run.id);
    }

    let is_active = !matches!(run.status, RunStatus::Complete | RunStatus::Failed);

    if is_active {
        tail_log(&log_file, args.agent.as_deref()).await?;
    } else {
        dump_log(&log_file, args.agent.as_deref()).await?;
    }

    Ok(())
}

async fn dump_log(path: &Path, agent_filter: Option<&str>) -> Result<()> {
    let content = tokio::fs::read_to_string(path).await.context("reading log file")?;

    for line in content.lines() {
        if should_show_line(line, agent_filter) {
            println!("{line}");
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
                        // EOF, wait briefly for more content
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if should_show_line(trimmed, agent_filter) {
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

fn should_show_line(line: &str, agent_filter: Option<&str>) -> bool {
    agent_filter
        .is_none_or(|agent| line.contains(&format!("agent={agent}")) || line.contains(agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_agent_field() {
        assert!(should_show_line(r#"{"agent":"reviewer","msg":"ok"}"#, Some("reviewer")));
        assert!(!should_show_line(r#"{"agent":"implementer","msg":"ok"}"#, Some("reviewer")));
    }

    #[test]
    fn no_filter_shows_all() {
        assert!(should_show_line("any line at all", None));
    }

    #[test]
    fn filter_matches_substring() {
        assert!(should_show_line("agent=reviewer cycle=1", Some("reviewer")));
    }
}
