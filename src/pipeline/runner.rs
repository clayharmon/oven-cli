use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::executor::PipelineExecutor;
use crate::{
    agents::{InFlightIssue, PlannerOutput},
    issues::PipelineIssue,
    process::CommandRunner,
};

/// Run the pipeline for a batch of issues using planner-driven sequencing.
///
/// Used for the explicit-IDs path (`oven on 42,43`). Calls the planner with no
/// in-flight context, then runs batches sequentially (issues within each batch
/// run in parallel). Falls back to all-parallel if the planner fails.
pub async fn run_batch<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<PipelineIssue>,
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    if let Some(plan) = executor.plan_issues(&issues, &[]).await {
        info!(
            batches = plan.batches.len(),
            total = plan.total_issues,
            "planner produced a plan, running batches sequentially"
        );
        run_batches_sequentially(executor, &issues, &plan, max_parallel, auto_merge).await
    } else {
        warn!("planner failed, falling back to all-parallel execution");
        run_all_parallel(executor, issues, max_parallel, auto_merge).await
    }
}

/// Run planner batches in sequence: wait for batch N to complete before starting batch N+1.
/// Issues within each batch run in parallel.
async fn run_batches_sequentially<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: &[PipelineIssue],
    plan: &PlannerOutput,
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    let issue_map: HashMap<u32, &PipelineIssue> = issues.iter().map(|i| (i.number, i)).collect();

    for batch in &plan.batches {
        let batch_issues: Vec<PipelineIssue> = batch
            .issues
            .iter()
            .filter_map(|pi| issue_map.get(&pi.number).map(|i| (*i).clone()))
            .collect();

        if batch_issues.is_empty() {
            continue;
        }

        info!(
            batch = batch.batch,
            count = batch_issues.len(),
            reasoning = %batch.reasoning,
            "starting batch"
        );

        run_single_batch(executor, batch_issues, &batch.issues, max_parallel, auto_merge).await?;
    }

    Ok(())
}

/// Run a single batch of issues in parallel with complexity from planner output.
async fn run_single_batch<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<PipelineIssue>,
    planned: &[crate::agents::PlannedIssue],
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    let complexity_map: HashMap<u32, crate::agents::Complexity> =
        planned.iter().map(|pi| (pi.number, pi.complexity.clone())).collect();
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
            Ok((number, Err(e))) => {
                error!(issue = number, error = %e, "pipeline failed for issue");
                had_errors = true;
            }
            Err(e) => {
                error!(error = %e, "pipeline task panicked");
                had_errors = true;
            }
            Ok((number, Ok(()))) => {
                info!(issue = number, "pipeline completed successfully");
            }
        }
    }

    if had_errors { Err(anyhow::anyhow!("one or more pipelines failed in batch")) } else { Ok(()) }
}

/// Fallback: run all issues in parallel behind a semaphore (no planner guidance).
async fn run_all_parallel<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<PipelineIssue>,
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
///
/// The planner receives in-flight metadata so it can avoid scheduling conflicting work
/// in batch 1. Only batch 1 issues are spawned each cycle; deferred issues keep `o-ready`
/// and naturally reappear on the next poll.
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
    let in_flight: Arc<Mutex<HashMap<u32, InFlightIssue>>> = Arc::new(Mutex::new(HashMap::new()));

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
                poll_and_spawn(
                    &executor, &ready_label, &semaphore, &in_flight,
                    &mut tasks, auto_merge,
                ).await;
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                handle_task_result(result);
            }
        }
    }

    Ok(())
}

/// Single poll cycle: fetch ready issues, plan, and spawn batch 1.
async fn poll_and_spawn<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    ready_label: &str,
    semaphore: &Arc<Semaphore>,
    in_flight: &Arc<Mutex<HashMap<u32, InFlightIssue>>>,
    tasks: &mut JoinSet<(u32, Result<()>)>,
    auto_merge: bool,
) {
    let issues = match executor.issues.get_ready_issues(ready_label).await {
        Ok(i) => i,
        Err(e) => {
            error!(error = %e, "failed to fetch issues");
            return;
        }
    };

    let in_flight_guard = in_flight.lock().await;
    let new_issues: Vec<_> =
        issues.into_iter().filter(|i| !in_flight_guard.contains_key(&i.number)).collect();
    let in_flight_snapshot: Vec<InFlightIssue> = in_flight_guard.values().cloned().collect();
    drop(in_flight_guard);

    if new_issues.is_empty() {
        info!("no new issues found, waiting");
        return;
    }

    info!(count = new_issues.len(), "found new issues to process");

    let (batch1_issues, metadata_map) =
        if let Some(plan) = executor.plan_issues(&new_issues, &in_flight_snapshot).await {
            info!(
                batches = plan.batches.len(),
                total = plan.total_issues,
                "planner produced a plan, spawning batch 1 only"
            );
            extract_batch1(&plan)
        } else {
            warn!("planner failed, falling back to spawning all issues");
            let all: HashMap<u32, InFlightIssue> =
                new_issues.iter().map(|i| (i.number, InFlightIssue::from_issue(i))).collect();
            let numbers: Vec<u32> = all.keys().copied().collect();
            (numbers, all)
        };

    for issue in new_issues {
        if !batch1_issues.contains(&issue.number) {
            info!(issue = issue.number, "deferring issue to next poll cycle (not in batch 1)");
            continue;
        }

        let sem = Arc::clone(semaphore);
        let exec = Arc::clone(executor);
        let in_fl = Arc::clone(in_flight);
        let number = issue.number;
        let complexity = metadata_map.get(&number).map(|m| m.complexity.clone());

        let metadata =
            metadata_map.get(&number).cloned().unwrap_or_else(|| InFlightIssue::from_issue(&issue));
        in_fl.lock().await.insert(number, metadata);

        tasks.spawn(async move {
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    in_fl.lock().await.remove(&number);
                    return (number, Err(anyhow::anyhow!("semaphore closed: {e}")));
                }
            };
            let result = exec.run_issue_with_complexity(&issue, auto_merge, complexity).await;
            in_fl.lock().await.remove(&number);
            drop(permit);
            (number, result)
        });
    }
}

/// Extract batch 1 issue numbers and their planner metadata from a planner output.
fn extract_batch1(plan: &PlannerOutput) -> (Vec<u32>, HashMap<u32, InFlightIssue>) {
    let mut batch1_numbers = Vec::new();
    let mut metadata_map = HashMap::new();

    if let Some(batch) = plan.batches.first() {
        for pi in &batch.issues {
            batch1_numbers.push(pi.number);
            metadata_map.insert(pi.number, InFlightIssue::from(pi));
        }
    }

    (batch1_numbers, metadata_map)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        agents::{Complexity, InFlightIssue},
        config::Config,
        github::GhClient,
        issues::{IssueOrigin, IssueProvider, github::GithubIssueProvider},
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
        mock.expect_run_claude().returning(|_, _, _, _| {
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

    fn make_github_provider(gh: &Arc<GhClient<MockCommandRunner>>) -> Arc<dyn IssueProvider> {
        Arc::new(GithubIssueProvider::new(Arc::clone(gh), "target_repo"))
    }

    #[tokio::test]
    async fn cancellation_stops_polling() {
        let cancel = CancellationToken::new();
        let runner = Arc::new(mock_runner_for_batch());
        let github = Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
        let issues = make_github_provider(&github);
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let mut config = Config::default();
        config.pipeline.poll_interval = 3600; // very long so we don't actually poll

        let executor = Arc::new(PipelineExecutor {
            runner,
            github,
            issues,
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
        let issues = make_github_provider(&github);
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let mut config = Config::default();
        config.pipeline.poll_interval = 3600;

        let executor = Arc::new(PipelineExecutor {
            runner,
            github,
            issues,
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
    async fn in_flight_map_filters_duplicate_issues() {
        let in_flight: Arc<Mutex<HashMap<u32, InFlightIssue>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Simulate issue 1 already in flight
        in_flight.lock().await.insert(
            1,
            InFlightIssue {
                number: 1,
                title: "Already running".to_string(),
                area: "auth".to_string(),
                predicted_files: vec!["src/auth.rs".to_string()],
                has_migration: false,
                complexity: Complexity::Full,
            },
        );

        let issues = vec![
            PipelineIssue {
                number: 1,
                title: "Already running".to_string(),
                body: String::new(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
            PipelineIssue {
                number: 2,
                title: "New issue".to_string(),
                body: String::new(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
            PipelineIssue {
                number: 3,
                title: "Another new".to_string(),
                body: String::new(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
        ];

        let guard = in_flight.lock().await;
        let new_issues: Vec<_> =
            issues.into_iter().filter(|i| !guard.contains_key(&i.number)).collect();
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

    #[test]
    fn extract_batch1_returns_first_batch_only() {
        let plan = crate::agents::PlannerOutput {
            batches: vec![
                crate::agents::Batch {
                    batch: 1,
                    issues: vec![
                        crate::agents::PlannedIssue {
                            number: 1,
                            title: "First".to_string(),
                            area: "cli".to_string(),
                            predicted_files: vec!["src/cli.rs".to_string()],
                            has_migration: false,
                            complexity: Complexity::Simple,
                        },
                        crate::agents::PlannedIssue {
                            number: 2,
                            title: "Second".to_string(),
                            area: "config".to_string(),
                            predicted_files: vec!["src/config.rs".to_string()],
                            has_migration: false,
                            complexity: Complexity::Full,
                        },
                    ],
                    reasoning: "independent".to_string(),
                },
                crate::agents::Batch {
                    batch: 2,
                    issues: vec![crate::agents::PlannedIssue {
                        number: 3,
                        title: "Third".to_string(),
                        area: "db".to_string(),
                        predicted_files: vec!["src/db.rs".to_string()],
                        has_migration: true,
                        complexity: Complexity::Full,
                    }],
                    reasoning: "depends on batch 1".to_string(),
                },
            ],
            total_issues: 3,
            parallel_capacity: 2,
        };

        let (batch1_numbers, metadata_map) = extract_batch1(&plan);
        assert_eq!(batch1_numbers, vec![1, 2]);
        assert!(!batch1_numbers.contains(&3));
        assert_eq!(metadata_map.get(&1).unwrap().complexity, Complexity::Simple);
        assert_eq!(metadata_map.get(&1).unwrap().area, "cli");
        assert_eq!(metadata_map.get(&2).unwrap().complexity, Complexity::Full);
        assert!(!metadata_map.contains_key(&3));
    }

    #[test]
    fn extract_batch1_empty_plan() {
        let plan =
            crate::agents::PlannerOutput { batches: vec![], total_issues: 0, parallel_capacity: 0 };
        let (batch1, metadata) = extract_batch1(&plan);
        assert!(batch1.is_empty());
        assert!(metadata.is_empty());
    }

    #[tokio::test]
    async fn planner_failure_falls_back_to_all_parallel() {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true })
            })
        });
        mock.expect_run_claude().returning(|_, _, _, _| {
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
        let issues_provider = make_github_provider(&github);
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        let executor = Arc::new(PipelineExecutor {
            runner,
            github,
            issues: issues_provider,
            db,
            config: Config::default(),
            cancel_token: CancellationToken::new(),
            repo_dir: PathBuf::from("/tmp"),
        });

        let issues = vec![PipelineIssue {
            number: 1,
            title: "Test".to_string(),
            body: "body".to_string(),
            source: IssueOrigin::Github,
            target_repo: None,
        }];

        // plan_issues returns None for unparseable output
        let plan = executor.plan_issues(&issues, &[]).await;
        assert!(plan.is_none());
    }
}
