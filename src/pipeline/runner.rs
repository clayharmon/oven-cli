use std::{sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::executor::PipelineExecutor;
use crate::{agents::Complexity, github::Issue, process::CommandRunner};

/// Run the pipeline for a batch of issues, limiting parallelism with a semaphore.
pub async fn run_batch<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<Issue>,
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut tasks = JoinSet::new();

    for issue in issues {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("semaphore closed: {e}"))?;
        let exec = Arc::clone(executor);
        tasks.spawn(async move {
            let number = issue.number;
            let result = exec.run_issue(&issue, auto_merge).await;
            drop(permit);
            (number, result)
        });
    }

    let mut had_errors = false;
    while let Some(join_result) = tasks.join_next().await {
        match join_result {
            Ok((number, Ok(()))) => {
                info!(issue = number, "pipeline completed successfully");
            }
            Ok((number, Err(e))) => {
                error!(issue = number, error = %e, "pipeline failed for issue");
                had_errors = true;
            }
            Err(e) => {
                error!(error = %e, "pipeline task panicked");
                had_errors = true;
            }
        }
    }

    if had_errors {
        anyhow::bail!("one or more pipelines failed");
    }

    Ok(())
}

/// Invoke the planner to decide batching, then execute batches sequentially.
///
/// Falls back to a single batch with `full` complexity if the planner fails.
async fn plan_and_run<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<Issue>,
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    match executor.plan_issues(&issues).await {
        Some(plan) => {
            info!(
                batches = plan.batches.len(),
                total = plan.total_issues,
                "planner produced a plan"
            );
            for batch in &plan.batches {
                let batch_issues: Vec<_> = batch
                    .issues
                    .iter()
                    .filter_map(|planned| {
                        issues.iter().find(|i| i.number == planned.number).cloned()
                    })
                    .collect();

                // Build a complexity map for this batch
                let complexity_map: std::collections::HashMap<u32, Complexity> = batch
                    .issues
                    .iter()
                    .map(|pi| (pi.number, pi.complexity.clone()))
                    .collect();

                run_batch_with_complexity(
                    executor,
                    batch_issues,
                    max_parallel,
                    auto_merge,
                    &complexity_map,
                )
                .await?;
            }
            Ok(())
        }
        None => {
            warn!("planner unavailable, running all issues in a single batch");
            run_batch(executor, issues, max_parallel, auto_merge).await
        }
    }
}

/// Run a batch of issues with per-issue complexity classification.
async fn run_batch_with_complexity<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<Issue>,
    max_parallel: usize,
    auto_merge: bool,
    complexity_map: &std::collections::HashMap<u32, Complexity>,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut tasks = JoinSet::new();

    for issue in issues {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("semaphore closed: {e}"))?;
        let exec = Arc::clone(executor);
        let complexity = complexity_map.get(&issue.number).cloned();
        tasks.spawn(async move {
            let number = issue.number;
            let result = exec.run_issue_with_complexity(&issue, auto_merge, complexity).await;
            drop(permit);
            (number, result)
        });
    }

    let mut had_errors = false;
    while let Some(join_result) = tasks.join_next().await {
        match join_result {
            Ok((number, Ok(()))) => {
                info!(issue = number, "pipeline completed successfully");
            }
            Ok((number, Err(e))) => {
                error!(issue = number, error = %e, "pipeline failed for issue");
                had_errors = true;
            }
            Err(e) => {
                error!(error = %e, "pipeline task panicked");
                had_errors = true;
            }
        }
    }

    if had_errors {
        anyhow::bail!("one or more pipelines failed");
    }

    Ok(())
}

/// Poll for new issues and run them through the pipeline.
pub async fn polling_loop<R: CommandRunner + 'static>(
    executor: Arc<PipelineExecutor<R>>,
    auto_merge: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    let poll_interval = Duration::from_secs(executor.config.pipeline.poll_interval);
    let max_parallel = executor.config.pipeline.max_parallel as usize;
    let ready_label = executor.config.labels.ready.clone();

    info!(poll_interval_secs = poll_interval.as_secs(), "polling started");

    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                info!("shutdown signal received, stopping poll loop");
                break;
            }
            () = tokio::time::sleep(poll_interval) => {
                match executor.github.get_issues_by_label(&ready_label).await {
                    Ok(issues) if issues.is_empty() => {
                        info!("no issues found, waiting");
                    }
                    Ok(issues) => {
                        info!(count = issues.len(), "found issues to process");
                        if let Err(e) = plan_and_run(&executor, issues, max_parallel, auto_merge).await {
                            error!(error = %e, "batch failed");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "failed to fetch issues");
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        config::Config,
        github::GhClient,
        process::{AgentResult, CommandOutput, MockCommandRunner},
    };

    fn mock_runner_for_batch() -> MockCommandRunner {
        let mut mock = MockCommandRunner::new();
        // The executor will call multiple operations; we need generic success responses
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: "https://github.com/user/repo/pull/1\n".to_string(),
                    stderr: String::new(),
                    success: true,
                })
            })
        });
        mock.expect_run_claude().returning(|_, _, _| {
            Box::pin(async {
                Ok(AgentResult {
                    cost_usd: 1.0,
                    duration: Duration::from_secs(5),
                    turns: 3,
                    output: r#"{"findings":[],"summary":"clean"}"#.to_string(),
                    session_id: "sess-1".to_string(),
                    success: true,
                })
            })
        });
        mock
    }

    #[tokio::test]
    async fn cancellation_stops_polling() {
        let cancel = CancellationToken::new();
        let runner = Arc::new(mock_runner_for_batch());
        let github = Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let mut config = Config::default();
        config.pipeline.poll_interval = 3600; // very long so we don't actually poll

        let executor = Arc::new(PipelineExecutor {
            runner,
            github,
            db,
            config,
            cancel_token: cancel.clone(),
            repo_dir: PathBuf::from("/tmp"),
        });

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { polling_loop(executor, false, cancel_clone).await });

        // Cancel immediately
        cancel.cancel();

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
}
