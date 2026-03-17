use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{
    executor::{PipelineExecutor, PipelineOutcome},
    graph::DependencyGraph,
};
use crate::{
    agents::Complexity,
    db::graph::NodeState,
    issues::PipelineIssue,
    pipeline::{executor::generate_run_id, graph::GraphNode},
    process::CommandRunner,
};

/// Shared mutable state for the polling scheduler.
///
/// The `DependencyGraph` is the single source of truth for issue states,
/// dependency edges, and scheduling decisions.
struct SchedulerState {
    graph: DependencyGraph,
    semaphore: Arc<Semaphore>,
    tasks: JoinSet<(u32, Result<PipelineOutcome>)>,
}

/// Run the pipeline for a batch of issues using planner-driven sequencing.
///
/// Used for the explicit-IDs path (`oven on 42,43`). Calls the planner with no
/// in-flight context, builds a `DependencyGraph`, then runs layers sequentially
/// (issues within each layer run in parallel). Falls back to all-parallel if the
/// planner fails.
pub async fn run_batch<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<PipelineIssue>,
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    let session_id = generate_run_id();
    let mut graph = if let Some(plan) = executor.plan_issues(&issues, &[]).await {
        info!(nodes = plan.nodes.len(), total = plan.total_issues, "planner produced a plan");
        DependencyGraph::from_planner_output(&session_id, &plan, &issues)
    } else {
        warn!("planner failed, falling back to all-parallel execution");
        let mut g = DependencyGraph::new(&session_id);
        for issue in &issues {
            g.add_node(standalone_node(issue));
        }
        g
    };

    save_graph(&graph, executor).await;

    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut had_errors = false;

    while !graph.all_terminal() {
        let ready = graph.ready_issues();
        if ready.is_empty() {
            warn!("no ready issues but graph is not terminal, breaking to avoid infinite loop");
            break;
        }

        let mut tasks: JoinSet<(u32, Result<PipelineOutcome>)> = JoinSet::new();

        for num in &ready {
            graph.transition(*num, NodeState::InFlight);
        }
        save_graph(&graph, executor).await;

        for num in ready {
            let node = graph.node(num).expect("ready issue must exist in graph");
            let issue = node.issue.clone().expect("batch issues have issue attached");
            let complexity = node.complexity.parse::<Complexity>().ok();
            let sem = Arc::clone(&semaphore);
            let exec = Arc::clone(executor);

            tasks.spawn(async move {
                let permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(e) => return (num, Err(anyhow::anyhow!("semaphore closed: {e}"))),
                };
                let result = exec.run_issue_pipeline(&issue, auto_merge, complexity).await;
                let outcome = match result {
                    Ok(outcome) => {
                        if let Err(e) = exec.finalize_merge(&outcome, &issue).await {
                            warn!(issue = num, error = %e, "finalize_merge failed");
                        }
                        Ok(outcome)
                    }
                    Err(e) => Err(e),
                };
                drop(permit);
                (num, outcome)
            });
        }

        while let Some(join_result) = tasks.join_next().await {
            match join_result {
                Ok((number, Ok(ref outcome))) => {
                    info!(issue = number, "pipeline completed successfully");
                    graph.set_pr_number(number, outcome.pr_number);
                    graph.set_run_id(number, &outcome.run_id);
                    graph.transition(number, NodeState::Merged);
                }
                Ok((number, Err(ref e))) => {
                    error!(issue = number, error = %e, "pipeline failed for issue");
                    graph.transition(number, NodeState::Failed);
                    let blocked = graph.propagate_failure(number);
                    for b in &blocked {
                        warn!(issue = b, blocked_by = number, "transitively failed");
                    }
                    had_errors = true;
                }
                Err(e) => {
                    error!(error = %e, "pipeline task panicked");
                    had_errors = true;
                }
            }
        }

        save_graph(&graph, executor).await;
    }

    if had_errors {
        anyhow::bail!("one or more pipelines failed in batch");
    }
    Ok(())
}

/// Poll for new issues and run them through the pipeline.
///
/// Unlike `run_batch`, this function continuously polls for new issues even while
/// existing pipelines are running. The `DependencyGraph` is the single source of
/// truth: `ready_issues()` drives scheduling, `transition()` replaces manual map
/// mutations, and `propagate_failure()` handles dependency cascades.
pub async fn polling_loop<R: CommandRunner + 'static>(
    executor: Arc<PipelineExecutor<R>>,
    auto_merge: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    let poll_interval = Duration::from_secs(executor.config.pipeline.poll_interval);
    let max_parallel = executor.config.pipeline.max_parallel as usize;
    let ready_label = executor.config.labels.ready.clone();

    // Try loading an existing graph session (crash recovery), or create a new one.
    let graph = load_or_create_graph(&executor).await;

    let mut sched = SchedulerState {
        graph,
        semaphore: Arc::new(Semaphore::new(max_parallel)),
        tasks: JoinSet::new(),
    };

    info!(poll_interval_secs = poll_interval.as_secs(), max_parallel, "continuous polling started");

    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                info!("shutdown signal received, waiting for in-flight pipelines");
                drain_tasks(&mut sched, &executor).await;
                break;
            }
            () = tokio::time::sleep(poll_interval) => {
                poll_and_spawn(&executor, &ready_label, &mut sched, auto_merge).await;
            }
            Some(result) = sched.tasks.join_next(), if !sched.tasks.is_empty() => {
                handle_task_result(result, &mut sched.graph, &executor).await;
            }
        }
    }

    Ok(())
}

/// Load an existing active graph session from DB, or create a new empty one.
async fn load_or_create_graph<R: CommandRunner>(
    executor: &Arc<PipelineExecutor<R>>,
) -> DependencyGraph {
    let conn = executor.db.lock().await;
    match crate::db::graph::get_active_session(&conn) {
        Ok(Some(session_id)) => match DependencyGraph::from_db(&conn, &session_id) {
            Ok(graph) => {
                info!(session_id = %session_id, nodes = graph.node_count(), "resumed existing graph session");
                return graph;
            }
            Err(e) => {
                warn!(error = %e, "failed to load graph session, starting fresh");
            }
        },
        Ok(None) => {}
        Err(e) => {
            warn!(error = %e, "failed to check for active graph session");
        }
    }
    let session_id = generate_run_id();
    info!(session_id = %session_id, "starting new graph session");
    DependencyGraph::new(&session_id)
}

/// Drain remaining tasks on shutdown.
async fn drain_tasks<R: CommandRunner>(
    sched: &mut SchedulerState,
    executor: &Arc<PipelineExecutor<R>>,
) {
    while let Some(result) = sched.tasks.join_next().await {
        handle_task_result(result, &mut sched.graph, executor).await;
    }
}

/// Process a completed pipeline task: update graph state and persist.
async fn handle_task_result<R: CommandRunner>(
    result: Result<(u32, Result<PipelineOutcome>), tokio::task::JoinError>,
    graph: &mut DependencyGraph,
    executor: &Arc<PipelineExecutor<R>>,
) {
    match result {
        Ok((number, Ok(ref outcome))) => {
            info!(issue = number, "pipeline completed successfully");
            graph.set_pr_number(number, outcome.pr_number);
            graph.set_run_id(number, &outcome.run_id);
            graph.transition(number, NodeState::AwaitingMerge);
        }
        Ok((number, Err(ref e))) => {
            error!(issue = number, error = %e, "pipeline failed for issue");
            graph.transition(number, NodeState::Failed);
            let blocked = graph.propagate_failure(number);
            for b in &blocked {
                warn!(issue = b, blocked_by = number, "transitively failed");
            }
        }
        Err(e) => {
            error!(error = %e, "pipeline task panicked");
            return;
        }
    }
    save_graph(graph, executor).await;
}

/// Check `AwaitingMerge` nodes and transition them to `Merged` or `Failed`
/// based on the PR's actual state on GitHub.
async fn poll_awaiting_merges<R: CommandRunner + 'static>(
    graph: &mut DependencyGraph,
    executor: &Arc<PipelineExecutor<R>>,
) {
    let awaiting = graph.awaiting_merge();
    if awaiting.is_empty() {
        return;
    }

    for num in awaiting {
        let Some(node) = graph.node(num) else { continue };
        let Some(pr_number) = node.pr_number else {
            warn!(issue = num, "AwaitingMerge node has no PR number, skipping");
            continue;
        };
        let run_id = node.run_id.clone().unwrap_or_default();
        let issue = node.issue.clone();

        let pr_state = match executor.github.get_pr_state(pr_number).await {
            Ok(s) => s,
            Err(e) => {
                warn!(issue = num, pr = pr_number, error = %e, "failed to check PR state");
                continue;
            }
        };

        match pr_state {
            crate::github::PrState::Merged => {
                info!(issue = num, pr = pr_number, "PR merged, finalizing");
                if let Some(ref issue) = issue {
                    match executor.reconstruct_outcome(issue, &run_id, pr_number) {
                        Ok(outcome) => {
                            if let Err(e) = executor.finalize_merge(&outcome, issue).await {
                                warn!(issue = num, error = %e, "finalize_merge after poll failed");
                            }
                        }
                        Err(e) => {
                            warn!(issue = num, error = %e, "failed to reconstruct outcome");
                        }
                    }
                }
                graph.transition(num, NodeState::Merged);
            }
            crate::github::PrState::Closed => {
                warn!(issue = num, pr = pr_number, "PR closed without merge, marking failed");
                graph.transition(num, NodeState::Failed);
                let blocked = graph.propagate_failure(num);
                for b in &blocked {
                    warn!(issue = b, blocked_by = num, "transitively failed (PR closed)");
                }
            }
            crate::github::PrState::Open => {
                // Still open, keep waiting
            }
        }
    }

    save_graph(graph, executor).await;
}

/// Single poll cycle: plan new issues, promote ready ones, and spawn tasks.
async fn poll_and_spawn<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    ready_label: &str,
    sched: &mut SchedulerState,
    auto_merge: bool,
) {
    // Check if any AwaitingMerge PRs have been merged
    poll_awaiting_merges(&mut sched.graph, executor).await;

    let ready_issues = match executor.issues.get_ready_issues(ready_label).await {
        Ok(i) => i,
        Err(e) => {
            error!(error = %e, "failed to fetch issues");
            return;
        }
    };

    let ready_numbers: HashSet<u32> = ready_issues.iter().map(|i| i.number).collect();

    // Clean stale nodes: remove Pending nodes whose issues disappeared from the ready list
    clean_stale_nodes(&mut sched.graph, &ready_numbers);

    // Filter to genuinely new issues not already in the graph
    let new_issues: Vec<_> =
        ready_issues.into_iter().filter(|i| !sched.graph.contains(i.number)).collect();

    // Plan and merge new issues into the graph
    if !new_issues.is_empty() {
        info!(count = new_issues.len(), "found new issues to evaluate");
        let graph_context = sched.graph.to_graph_context();

        if let Some(plan) = executor.plan_issues(&new_issues, &graph_context).await {
            info!(nodes = plan.nodes.len(), total = plan.total_issues, "planner produced a plan");
            sched.graph.merge_planner_output(&plan, &new_issues);
        } else {
            warn!("planner failed, adding all new issues as independent nodes");
            add_independent_nodes(&mut sched.graph, &new_issues);
        }

        save_graph(&sched.graph, executor).await;
    }

    // Spawn ready issues
    let to_spawn = collect_ready_issues(&mut sched.graph);
    if to_spawn.is_empty() {
        if new_issues.is_empty() {
            info!("no actionable issues, waiting");
        }
        return;
    }

    save_graph(&sched.graph, executor).await;
    spawn_issues(to_spawn, executor, sched, auto_merge);
}

/// Remove graph nodes that are still `Pending` but no longer in the provider's ready list.
fn clean_stale_nodes(graph: &mut DependencyGraph, ready_numbers: &HashSet<u32>) {
    let stale: Vec<u32> = graph
        .all_issues()
        .into_iter()
        .filter(|num| {
            !ready_numbers.contains(num)
                && graph.node(*num).is_some_and(|n| n.state == NodeState::Pending)
        })
        .collect();
    if !stale.is_empty() {
        info!(count = stale.len(), "removing stale pending nodes");
        for num in stale {
            graph.remove_node(num);
        }
    }
}

/// Add issues to the graph as independent nodes (no edges) when the planner fails.
fn add_independent_nodes(graph: &mut DependencyGraph, issues: &[PipelineIssue]) {
    for issue in issues {
        if !graph.contains(issue.number) {
            graph.add_node(standalone_node(issue));
        }
    }
}

/// Find ready issues in the graph, transition them to `InFlight`, return spawn data.
fn collect_ready_issues(graph: &mut DependencyGraph) -> Vec<(u32, PipelineIssue, Complexity)> {
    let ready = graph.ready_issues();
    let mut to_spawn = Vec::new();

    for num in ready {
        let Some(node) = graph.node(num) else { continue };
        let Some(issue) = node.issue.clone() else {
            warn!(issue = num, "ready node has no PipelineIssue attached, skipping");
            continue;
        };
        let complexity = node.complexity.parse::<Complexity>().unwrap_or(Complexity::Full);
        graph.transition(num, NodeState::InFlight);
        to_spawn.push((num, issue, complexity));
    }

    to_spawn
}

/// Spawn pipeline tasks for a set of issues.
fn spawn_issues<R: CommandRunner + 'static>(
    to_spawn: Vec<(u32, PipelineIssue, Complexity)>,
    executor: &Arc<PipelineExecutor<R>>,
    sched: &mut SchedulerState,
    auto_merge: bool,
) {
    for (number, issue, complexity) in to_spawn {
        let sem = Arc::clone(&sched.semaphore);
        let exec = Arc::clone(executor);

        sched.tasks.spawn(async move {
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(e) => return (number, Err(anyhow::anyhow!("semaphore closed: {e}"))),
            };
            let outcome = exec.run_issue_pipeline(&issue, auto_merge, Some(complexity)).await;
            drop(permit);
            (number, outcome)
        });
    }
}

/// Create a `GraphNode` for an issue with no planner metadata.
fn standalone_node(issue: &PipelineIssue) -> GraphNode {
    GraphNode {
        issue_number: issue.number,
        title: issue.title.clone(),
        area: String::new(),
        predicted_files: Vec::new(),
        has_migration: false,
        complexity: Complexity::Full.to_string(),
        state: NodeState::Pending,
        pr_number: None,
        run_id: None,
        issue: Some(issue.clone()),
    }
}

/// Persist graph state to the database.
async fn save_graph<R: CommandRunner>(
    graph: &DependencyGraph,
    executor: &Arc<PipelineExecutor<R>>,
) {
    let conn = executor.db.lock().await;
    if let Err(e) = graph.save_to_db(&conn) {
        warn!(error = %e, "failed to persist dependency graph");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        agents::PlannerGraphOutput,
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

    fn make_issue(number: u32) -> PipelineIssue {
        PipelineIssue {
            number,
            title: format!("Issue #{number}"),
            body: String::new(),
            source: IssueOrigin::Github,
            target_repo: None,
        }
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

    #[test]
    fn handle_task_success_transitions_to_awaiting_merge() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let executor = {
                let runner = Arc::new(mock_runner_for_batch());
                let github =
                    Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
                let issues = make_github_provider(&github);
                let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
                Arc::new(PipelineExecutor {
                    runner,
                    github,
                    issues,
                    db,
                    config: Config::default(),
                    cancel_token: CancellationToken::new(),
                    repo_dir: PathBuf::from("/tmp"),
                })
            };

            let mut graph = DependencyGraph::new("test");
            graph.add_node(standalone_node(&make_issue(1)));
            graph.transition(1, NodeState::InFlight);

            let outcome = PipelineOutcome {
                run_id: "run-abc".to_string(),
                pr_number: 42,
                worktree_path: PathBuf::from("/tmp/wt"),
                target_dir: PathBuf::from("/tmp"),
            };

            handle_task_result(Ok((1, Ok(outcome))), &mut graph, &executor).await;

            assert_eq!(graph.node(1).unwrap().state, NodeState::AwaitingMerge);
            assert_eq!(graph.node(1).unwrap().pr_number, Some(42));
            assert_eq!(graph.node(1).unwrap().run_id.as_deref(), Some("run-abc"));
        });
    }

    #[test]
    fn handle_task_failure_propagates_to_dependents() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let executor = {
                let runner = Arc::new(mock_runner_for_batch());
                let github =
                    Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
                let issues = make_github_provider(&github);
                let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
                Arc::new(PipelineExecutor {
                    runner,
                    github,
                    issues,
                    db,
                    config: Config::default(),
                    cancel_token: CancellationToken::new(),
                    repo_dir: PathBuf::from("/tmp"),
                })
            };

            let plan = PlannerGraphOutput {
                nodes: vec![
                    crate::agents::PlannedNode {
                        number: 1,
                        title: "Root".to_string(),
                        area: "a".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                        depends_on: vec![],
                        reasoning: String::new(),
                    },
                    crate::agents::PlannedNode {
                        number: 2,
                        title: "Dep".to_string(),
                        area: "b".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                        depends_on: vec![1],
                        reasoning: String::new(),
                    },
                ],
                total_issues: 2,
                parallel_capacity: 1,
            };
            let issues = vec![make_issue(1), make_issue(2)];
            let mut graph = DependencyGraph::from_planner_output("test", &plan, &issues);
            graph.transition(1, NodeState::InFlight);

            handle_task_result(
                Ok((1, Err(anyhow::anyhow!("pipeline failed")))),
                &mut graph,
                &executor,
            )
            .await;

            assert_eq!(graph.node(1).unwrap().state, NodeState::Failed);
            assert_eq!(graph.node(2).unwrap().state, NodeState::Failed);
        });
    }

    #[test]
    fn stale_node_removed_when_issue_disappears() {
        let mut graph = DependencyGraph::new("test");
        graph.add_node(standalone_node(&make_issue(1)));
        graph.add_node(standalone_node(&make_issue(2)));
        graph.add_node(standalone_node(&make_issue(3)));
        graph.transition(2, NodeState::InFlight);

        // Only issue 1 and 2 remain in provider; 3 disappeared
        let ready_numbers: HashSet<u32> = HashSet::from([1, 2]);
        clean_stale_nodes(&mut graph, &ready_numbers);

        assert!(graph.contains(1)); // still Pending + in ready list
        assert!(graph.contains(2)); // InFlight, not removed even if not in ready
        assert!(!graph.contains(3)); // Pending + not in ready = removed
    }

    #[test]
    fn collect_ready_issues_transitions_to_in_flight() {
        let mut graph = DependencyGraph::new("test");
        graph.add_node(standalone_node(&make_issue(1)));
        graph.add_node(standalone_node(&make_issue(2)));

        let spawnable = collect_ready_issues(&mut graph);
        assert_eq!(spawnable.len(), 2);

        // Both should now be InFlight
        assert_eq!(graph.node(1).unwrap().state, NodeState::InFlight);
        assert_eq!(graph.node(2).unwrap().state, NodeState::InFlight);

        // No more ready issues
        assert!(collect_ready_issues(&mut graph).is_empty());
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

    #[test]
    fn graph_persisted_after_state_change() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
            let runner = Arc::new(mock_runner_for_batch());
            let github =
                Arc::new(GhClient::new(mock_runner_for_batch(), std::path::Path::new("/tmp")));
            let issues = make_github_provider(&github);
            let executor = Arc::new(PipelineExecutor {
                runner,
                github,
                issues,
                db: Arc::clone(&db),
                config: Config::default(),
                cancel_token: CancellationToken::new(),
                repo_dir: PathBuf::from("/tmp"),
            });

            let mut graph = DependencyGraph::new("persist-test");
            graph.add_node(standalone_node(&make_issue(1)));
            graph.transition(1, NodeState::InFlight);

            let outcome = PipelineOutcome {
                run_id: "run-1".to_string(),
                pr_number: 10,
                worktree_path: PathBuf::from("/tmp/wt"),
                target_dir: PathBuf::from("/tmp"),
            };
            handle_task_result(Ok((1, Ok(outcome))), &mut graph, &executor).await;

            // Load from DB and verify
            let loaded = DependencyGraph::from_db(&*db.lock().await, "persist-test").unwrap();
            assert_eq!(loaded.node(1).unwrap().state, NodeState::AwaitingMerge);
            assert_eq!(loaded.node(1).unwrap().pr_number, Some(10));
        });
    }

    fn mock_runner_with_pr_state(state: &'static str) -> MockCommandRunner {
        let mut mock = MockCommandRunner::new();
        mock.expect_run_gh().returning(move |args, _| {
            let args = args.to_vec();
            Box::pin(async move {
                if args.iter().any(|a| a == "view") {
                    Ok(CommandOutput {
                        stdout: format!(r#"{{"state":"{state}"}}"#),
                        stderr: String::new(),
                        success: true,
                    })
                } else {
                    Ok(CommandOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        success: true,
                    })
                }
            })
        });
        mock.expect_run_claude().returning(|_, _, _, _| {
            Box::pin(async {
                Ok(AgentResult {
                    cost_usd: 0.0,
                    duration: Duration::from_secs(0),
                    turns: 0,
                    output: String::new(),
                    session_id: String::new(),
                    success: true,
                })
            })
        });
        mock
    }

    fn make_merge_poll_executor(state: &'static str) -> Arc<PipelineExecutor<MockCommandRunner>> {
        let gh_mock = mock_runner_with_pr_state(state);
        let github = Arc::new(GhClient::new(gh_mock, std::path::Path::new("/tmp")));
        let issues = make_github_provider(&github);
        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
        let runner = Arc::new(mock_runner_with_pr_state(state));
        Arc::new(PipelineExecutor {
            runner,
            github,
            issues,
            db,
            config: Config::default(),
            cancel_token: CancellationToken::new(),
            repo_dir: PathBuf::from("/tmp"),
        })
    }

    #[test]
    fn merge_polling_transitions_merged_pr() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let executor = make_merge_poll_executor("MERGED");

            let mut graph = DependencyGraph::new("merge-poll-test");
            let mut node = standalone_node(&make_issue(1));
            node.pr_number = Some(42);
            node.run_id = Some("run-1".to_string());
            graph.add_node(node);
            graph.transition(1, NodeState::AwaitingMerge);

            poll_awaiting_merges(&mut graph, &executor).await;

            assert_eq!(graph.node(1).unwrap().state, NodeState::Merged);
        });
    }

    #[test]
    fn merge_polling_handles_closed_pr() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let executor = make_merge_poll_executor("CLOSED");

            let plan = PlannerGraphOutput {
                nodes: vec![
                    crate::agents::PlannedNode {
                        number: 1,
                        title: "Root".to_string(),
                        area: "a".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                        depends_on: vec![],
                        reasoning: String::new(),
                    },
                    crate::agents::PlannedNode {
                        number: 2,
                        title: "Dep".to_string(),
                        area: "b".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                        depends_on: vec![1],
                        reasoning: String::new(),
                    },
                ],
                total_issues: 2,
                parallel_capacity: 1,
            };
            let test_issues = vec![make_issue(1), make_issue(2)];
            let mut graph =
                DependencyGraph::from_planner_output("merge-poll-close", &plan, &test_issues);
            graph.transition(1, NodeState::AwaitingMerge);
            graph.set_pr_number(1, 42);
            graph.set_run_id(1, "run-1");

            poll_awaiting_merges(&mut graph, &executor).await;

            assert_eq!(graph.node(1).unwrap().state, NodeState::Failed);
            // Dependent should be transitively failed
            assert_eq!(graph.node(2).unwrap().state, NodeState::Failed);
        });
    }

    #[test]
    fn merge_unlocks_dependent() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let executor = make_merge_poll_executor("MERGED");

            let plan = PlannerGraphOutput {
                nodes: vec![
                    crate::agents::PlannedNode {
                        number: 1,
                        title: "Root".to_string(),
                        area: "a".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                        depends_on: vec![],
                        reasoning: String::new(),
                    },
                    crate::agents::PlannedNode {
                        number: 2,
                        title: "Dep".to_string(),
                        area: "b".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                        depends_on: vec![1],
                        reasoning: String::new(),
                    },
                ],
                total_issues: 2,
                parallel_capacity: 1,
            };
            let test_issues = vec![make_issue(1), make_issue(2)];
            let mut graph =
                DependencyGraph::from_planner_output("merge-unlock", &plan, &test_issues);
            graph.transition(1, NodeState::AwaitingMerge);
            graph.set_pr_number(1, 42);
            graph.set_run_id(1, "run-1");

            // Before polling: node 2 is not ready (dep 1 is AwaitingMerge)
            assert!(graph.ready_issues().is_empty());

            poll_awaiting_merges(&mut graph, &executor).await;

            // After polling: node 1 merged, node 2 should now be ready
            assert_eq!(graph.node(1).unwrap().state, NodeState::Merged);
            assert_eq!(graph.ready_issues(), vec![2]);
        });
    }
}
