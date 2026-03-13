use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{GlobalOpts, OnArgs};
use crate::{
    config::{Config, IssueSource},
    github::GhClient,
    issues::{IssueProvider, github::GithubIssueProvider, local::LocalIssueProvider},
    pipeline::{executor::PipelineExecutor, runner},
    process::RealCommandRunner,
};

pub async fn run(args: OnArgs, global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
    let config = Config::load(&project_dir)?;

    let run_id = args.run_id.clone().unwrap_or_else(crate::pipeline::executor::generate_run_id);

    // Detached mode: re-spawn self without -d flag
    if args.detached {
        return spawn_detached(&project_dir, &args, &run_id);
    }

    // Set up logging
    let log_dir = project_dir.join(".oven").join("logs").join(&run_id);
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("creating log dir: {}", log_dir.display()))?;
    let _guard = crate::logging::init_with_file(&log_dir, global.verbose);

    println!("{run_id}");

    let cancel_token = CancellationToken::new();
    let cancel_for_signal = cancel_token.clone();

    // Signal handler
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received ctrl-c, shutting down");
            cancel_for_signal.cancel();
        }
    });

    let runner = Arc::new(RealCommandRunner);
    let github = Arc::new(GhClient::new(RealCommandRunner, &project_dir));
    let db_path = project_dir.join(".oven").join("oven.db");
    let conn = crate::db::open(&db_path)?;
    let db = Arc::new(Mutex::new(conn));

    // Build the issue provider based on config
    let issues: Arc<dyn IssueProvider> = match config.project.issue_source {
        IssueSource::Github => {
            Arc::new(GithubIssueProvider::new(Arc::clone(&github), &config.multi_repo.target_field))
        }
        IssueSource::Local => Arc::new(LocalIssueProvider::new(&project_dir)),
    };

    let executor = Arc::new(PipelineExecutor {
        runner,
        github,
        issues: Arc::clone(&issues),
        db,
        config: config.clone(),
        cancel_token: cancel_token.clone(),
        repo_dir: project_dir,
    });

    if let Some(ids_str) = &args.ids {
        // Run specific issues
        let ids = parse_issue_ids(ids_str)?;
        let mut fetched = Vec::new();
        for id in ids {
            let issue = issues.get_issue(id).await?;
            fetched.push(issue);
        }

        runner::run_batch(&executor, fetched, config.pipeline.max_parallel as usize, args.merge)
            .await?;
    } else {
        // Polling mode
        runner::polling_loop(executor, args.merge, cancel_token).await?;
    }

    Ok(())
}

fn parse_issue_ids(ids: &str) -> Result<Vec<u32>> {
    ids.split(',')
        .map(|s| s.trim().parse::<u32>().with_context(|| format!("invalid issue number: {s}")))
        .collect()
}

fn spawn_detached(project_dir: &std::path::Path, args: &OnArgs, run_id: &str) -> Result<()> {
    let exe = std::env::current_exe().context("finding current executable")?;

    let mut cmd_args = vec!["on".to_string()];
    if let Some(ref ids) = args.ids {
        cmd_args.push(ids.clone());
    }
    if args.merge {
        cmd_args.push("-m".to_string());
    }
    cmd_args.extend(["--run-id".to_string(), run_id.to_string()]);

    let log_dir = project_dir.join(".oven").join("logs");
    std::fs::create_dir_all(&log_dir).context("creating log dir for detached")?;

    let stdout = std::fs::File::create(log_dir.join("detached.stdout"))
        .context("creating detached stdout")?;
    let stderr = std::fs::File::create(log_dir.join("detached.stderr"))
        .context("creating detached stderr")?;

    let child = std::process::Command::new(exe)
        .args(&cmd_args)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("spawning detached process")?;

    let pid_path = project_dir.join(".oven").join("oven.pid");
    std::fs::write(&pid_path, child.id().to_string()).context("writing PID file")?;

    println!("{run_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_id() {
        let ids = parse_issue_ids("42").unwrap();
        assert_eq!(ids, vec![42]);
    }

    #[test]
    fn parse_multiple_ids() {
        let ids = parse_issue_ids("1,2,3").unwrap();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn parse_ids_with_spaces() {
        let ids = parse_issue_ids("1, 2, 3").unwrap();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn parse_invalid_id_fails() {
        let result = parse_issue_ids("1,abc,3");
        assert!(result.is_err());
    }
}
