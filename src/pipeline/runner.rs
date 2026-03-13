use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use super::executor::PipelineExecutor;
use crate::{agents::Complexity, github::Issue, process::CommandRunner};

/// Run the pipeline for a batch of issues, limiting parallelism with a semaphore.
///
/// Used for the explicit-IDs path (`oven on 42,43`). For the polling path, see
/// [`polling_loop`] which handles continuous issue discovery.
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

/// Extract per-issue complexity from the planner, if available.
///
/// Returns an empty map if the planner fails or returns unparseable output.
async fn get_complexity_map<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: &[Issue],
) -> HashMap<u32, Complexity> {
    let mut map = HashMap::new();
    if let Some(plan) = executor.plan_issues(issues).await {
        info!(batches = plan.batches.len(), total = plan.total_issues, "planner produced a plan");
        for batch in &plan.batches {
            for pi in &batch.issues {
                map.insert(pi.number, pi.complexity.clone());
            }
        }
    }
    map
}

fn handle_task_result(result: Result<(u32, Result<()>), tokio::task::JoinError>) {
    match result {
        Ok((number, Ok(()))) => {
            info!(issue = number, "pipeline completed successfully");
        }
        Ok((number, Err(e))) => {
            error!(issue = number, error = %e, "pipeline failed for issue");
        }
        Err(e) => {
            error!(error = %e, "pipeline task panicked");
        }
    }
}

/// Poll for new issues and run them through the pipeline.
///
/// Unlike `run_batch`, this function continuously polls for new issues even while
/// existing pipelines are running. Uses a shared semaphore and `JoinSet` that persist
/// across poll cycles, with in-flight tracking to prevent double-spawning.
pub async fn polling_loop<R: CommandRunner + 'static>(
    executor: Arc<PipelineExecutor<R>>,
    auto_merge: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    let poll_interval = Duration::from_secs(executor.config.pipeline.poll_interval);
    let max_parallel = executor.config.pipeline.max_parallel as usize;
    let ready_label = executor.config.labels.ready.clone();
    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut tasks = JoinSet::new();
    let in_flight: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    info!(poll_interval_secs = poll_interval.as_secs(), max_parallel, "continuous polling started");

    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                info!("shutdown signal received, waiting for in-flight pipelines");
                while let Some(result) = tasks.join_next().await {
                    handle_task_result(result);
                }
                break;
            }
            () = tokio::time::sleep(poll_interval) => {
                match executor.github.get_issues_by_label(&ready_label).await {
                    Ok(issues) => {
                        let in_flight_guard = in_flight.lock().await;
                        let new_issues: Vec<_> = issues
                            .into_iter()
                            .filter(|i| !in_flight_guard.contains(&i.number))
                            .collect();
                        drop(in_flight_guard);

                        if new_issues.is_empty() {
                            info!("no new issues found, waiting");
                            continue;
                        }

                        info!(count = new_issues.len(), "found new issues to process");

                        let complexity_map =
                            get_complexity_map(&executor, &new_issues).await;

                        for issue in new_issues {
                            let sem = Arc::clone(&semaphore);
                            let exec = Arc::clone(&executor);
                            let in_fl = Arc::clone(&in_flight);
                            let number = issue.number;
                            let complexity = complexity_map.get(&number).cloned();

                            in_fl.lock().await.insert(number);

                            tasks.spawn(async move {
                                let permit = match sem.acquire_owned().await {
                                    Ok(p) => p,
                                    Err(e) => {
                                        in_fl.lock().await.remove(&number);
                                        return (
                                            number,
                                            Err(anyhow::anyhow!(
                                                "semaphore closed: {e}"
                                            )),
                                        );
                                    }
                                };
                                let result = exec
                                    .run_issue_with_complexity(
                                        &issue,
                                        auto_merge,
                                        complexity,
                                    )
                                    .await;
                                in_fl.lock().await.remove(&number);
                                drop(permit);
                                (number, result)
                            });
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "failed to fetch issues");
                    }
                }
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                handle_task_result(result);
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

    #[tokio::test]
    async fn cancellation_exits_within_timeout() {
        let cancel = CancellationToken::new();
        let runner = Arc::new(mock_runner_for_batch());
        let github = Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let mut config = Config::default();
        config.pipeline.poll_interval = 3600;

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

        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("polling loop should exit within timeout")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn in_flight_set_filters_duplicate_issues() {
        let in_flight: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

        // Simulate issue 1 already in flight
        in_flight.lock().await.insert(1);

        let issues = vec![
            Issue {
                number: 1,
                title: "Already running".to_string(),
                body: String::new(),
                labels: vec![],
            },
            Issue {
                number: 2,
                title: "New issue".to_string(),
                body: String::new(),
                labels: vec![],
            },
            Issue {
                number: 3,
                title: "Another new".to_string(),
                body: String::new(),
                labels: vec![],
            },
        ];

        let guard = in_flight.lock().await;
        let new_issues: Vec<_> =
            issues.into_iter().filter(|i| !guard.contains(&i.number)).collect();
        drop(guard);

        assert_eq!(new_issues.len(), 2);
        assert_eq!(new_issues[0].number, 2);
        assert_eq!(new_issues[1].number, 3);
    }

    #[test]
    fn handle_task_result_does_not_panic_on_success() {
        handle_task_result(Ok((1, Ok(()))));
    }

    #[test]
    fn handle_task_result_does_not_panic_on_error() {
        handle_task_result(Ok((1, Err(anyhow::anyhow!("test error")))));
    }

    #[tokio::test]
    async fn get_complexity_map_returns_empty_on_planner_failure() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });
        mock.expect_run_claude().returning(|_, _, _| {
            Box::pin(async {
                Ok(AgentResult {
                    cost_usd: 0.5,
                    duration: Duration::from_secs(2),
                    turns: 1,
                    output: "I don't know how to plan".to_string(),
                    session_id: "sess-plan".to_string(),
                    success: true,
                })
            })
        });

        let runner = Arc::new(mock);
        let github = Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let executor = Arc::new(PipelineExecutor {
            runner,
            github,
            db,
            config: Config::default(),
            cancel_token: CancellationToken::new(),
            repo_dir: PathBuf::from("/tmp"),
        });

        let issues = vec![Issue {
            number: 1,
            title: "Test".to_string(),
            body: "body".to_string(),
            labels: vec![],
        }];

        let map = get_complexity_map(&executor, &issues).await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn get_complexity_map_extracts_complexity() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });
        mock.expect_run_claude().returning(|_, _, _| {
            Box::pin(async {
                Ok(AgentResult {
                    cost_usd: 0.5,
                    duration: Duration::from_secs(2),
                    turns: 1,
                    output: r#"{"batches":[{"batch":1,"issues":[{"number":1,"complexity":"simple"},{"number":2,"complexity":"full"}],"reasoning":"ok"}],"total_issues":2,"parallel_capacity":2}"#.to_string(),
                    session_id: "sess-plan".to_string(),
                    success: true,
                })
            })
        });

        let runner = Arc::new(mock);
        let github = Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let executor = Arc::new(PipelineExecutor {
            runner,
            github,
            db,
            config: Config::default(),
            cancel_token: CancellationToken::new(),
            repo_dir: PathBuf::from("/tmp"),
        });

        let issues = vec![
            Issue {
                number: 1,
                title: "Simple".to_string(),
                body: "simple change".to_string(),
                labels: vec![],
            },
            Issue {
                number: 2,
                title: "Complex".to_string(),
                body: "big feature".to_string(),
                labels: vec![],
            },
        ];

        let map = get_complexity_map(&executor, &issues).await;
        assert_eq!(map.get(&1), Some(&Complexity::Simple));
        assert_eq!(map.get(&2), Some(&Complexity::Full));
    }
}
