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
use tracing::{error, info, warn};

use super::executor::{PipelineExecutor, PipelineOutcome};
use crate::{
    agents::{GraphContextNode, InFlightIssue, PlannerGraphOutput},
    issues::PipelineIssue,
    process::CommandRunner,
};

/// An issue the planner has evaluated and placed in a later batch.
///
/// Stored across poll cycles so we skip re-invoking the planner for issues whose
/// dependency chain is already known. The `awaiting` set tracks which issue numbers
/// must complete before this issue can be promoted to in-flight.
#[derive(Debug, Clone)]
struct DeferredIssue {
    issue: PipelineIssue,
    metadata: InFlightIssue,
    awaiting: HashSet<u32>,
}

/// Shared mutable state for the polling scheduler.
///
/// Groups the semaphore, in-flight map, deferred map, and task set that are
/// threaded through every poll cycle and spawn call.
struct SchedulerState {
    semaphore: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashMap<u32, InFlightIssue>>>,
    deferred: Arc<Mutex<HashMap<u32, DeferredIssue>>>,
    tasks: JoinSet<(u32, Result<PipelineOutcome>)>,
}

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
        info!(nodes = plan.nodes.len(), total = plan.total_issues, "planner produced a plan");
        run_dag_layers(executor, &issues, &plan, max_parallel, auto_merge).await
    } else {
        warn!("planner failed, falling back to all-parallel execution");
        run_issues_parallel(executor, issues, None, max_parallel, auto_merge).await
    }
}

/// Compute topological layers from a DAG: nodes with no unsatisfied dependencies form
/// layer 1, nodes depending only on layer 1 form layer 2, etc.
///
/// Each layer is a vec of `PlannedNode` references. Nodes with unresolvable dependencies
/// (cycles or missing nodes) are forced into the final layer.
fn topological_layers(plan: &PlannerGraphOutput) -> Vec<Vec<&crate::agents::PlannedNode>> {
    use std::collections::VecDeque;

    let mut layers = Vec::new();
    let mut assigned: HashSet<u32> = HashSet::new();
    let mut remaining: VecDeque<&crate::agents::PlannedNode> = plan.nodes.iter().collect();

    while !remaining.is_empty() {
        let mut current_layer = Vec::new();
        let mut next_remaining = VecDeque::new();

        for node in remaining {
            if node.depends_on.iter().all(|d| assigned.contains(d)) {
                current_layer.push(node);
            } else {
                next_remaining.push_back(node);
            }
        }

        if current_layer.is_empty() {
            // Remaining nodes have unresolvable deps, force them into final layer
            for node in next_remaining {
                current_layer.push(node);
            }
            next_remaining = VecDeque::new();
        }

        for node in &current_layer {
            assigned.insert(node.number);
        }

        layers.push(current_layer);
        remaining = next_remaining;
    }

    layers
}

/// Run DAG layers in sequence: wait for layer N to complete before starting layer N+1.
/// Issues within each layer run in parallel.
async fn run_dag_layers<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: &[PipelineIssue],
    plan: &PlannerGraphOutput,
    max_parallel: usize,
    auto_merge: bool,
) -> Result<()> {
    let issue_map: HashMap<u32, &PipelineIssue> = issues.iter().map(|i| (i.number, i)).collect();
    let layers = topological_layers(plan);

    for (idx, layer) in layers.iter().enumerate() {
        let layer_issues: Vec<PipelineIssue> = layer
            .iter()
            .filter_map(|node| issue_map.get(&node.number).map(|i| (*i).clone()))
            .collect();

        if layer_issues.is_empty() {
            continue;
        }

        let reasoning = layer.first().map_or("", |n| &n.reasoning);
        info!(
            layer = idx + 1,
            count = layer_issues.len(),
            reasoning = %reasoning,
            "starting layer"
        );

        let complexity_map: HashMap<u32, crate::agents::Complexity> =
            layer.iter().map(|n| (n.number, n.complexity.clone())).collect();
        run_issues_parallel(
            executor,
            layer_issues,
            Some(&complexity_map),
            max_parallel,
            auto_merge,
        )
        .await?;
    }

    Ok(())
}

/// Run a set of issues in parallel behind a semaphore. If `complexity_map` is provided,
/// each issue uses its planner-assigned complexity; otherwise complexity defaults to None.
async fn run_issues_parallel<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    issues: Vec<PipelineIssue>,
    complexity_map: Option<&HashMap<u32, crate::agents::Complexity>>,
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
        let complexity = complexity_map.and_then(|m| m.get(&issue.number).cloned());
        tasks.spawn(async move {
            let number = issue.number;
            let result = exec.run_issue_pipeline(&issue, auto_merge, complexity).await;
            let outcome = match result {
                Ok(outcome) => {
                    if let Err(e) = exec.finalize_merge(&outcome, &issue).await {
                        warn!(issue = number, error = %e, "finalize_merge failed");
                    }
                    Ok(outcome)
                }
                Err(e) => Err(e),
            };
            drop(permit);
            (number, outcome)
        });
    }

    let mut had_errors = false;
    while let Some(join_result) = tasks.join_next().await {
        match join_result {
            Ok((number, Ok(_outcome))) => {
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
        anyhow::bail!("one or more pipelines failed in batch");
    }
    Ok(())
}

fn handle_task_result(result: Result<(u32, Result<PipelineOutcome>), tokio::task::JoinError>) {
    match result {
        Ok((number, Ok(_outcome))) => {
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
/// across poll cycles, with in-flight and deferred tracking to prevent double-spawning
/// and avoid re-invoking the planner for issues whose dependency chain is already known.
///
/// Deferred issues (batch 2+) are stored locally and promoted automatically when their
/// dependencies complete, saving planner tokens on subsequent poll cycles.
pub async fn polling_loop<R: CommandRunner + 'static>(
    executor: Arc<PipelineExecutor<R>>,
    auto_merge: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    let poll_interval = Duration::from_secs(executor.config.pipeline.poll_interval);
    let max_parallel = executor.config.pipeline.max_parallel as usize;
    let ready_label = executor.config.labels.ready.clone();
    let mut sched = SchedulerState {
        semaphore: Arc::new(Semaphore::new(max_parallel)),
        in_flight: Arc::new(Mutex::new(HashMap::new())),
        deferred: Arc::new(Mutex::new(HashMap::new())),
        tasks: JoinSet::new(),
    };

    info!(poll_interval_secs = poll_interval.as_secs(), max_parallel, "continuous polling started");

    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                info!("shutdown signal received, waiting for in-flight pipelines");
                while let Some(result) = sched.tasks.join_next().await {
                    handle_task_result(result);
                }
                break;
            }
            () = tokio::time::sleep(poll_interval) => {
                poll_and_spawn(&executor, &ready_label, &mut sched, auto_merge).await;
            }
            Some(result) = sched.tasks.join_next(), if !sched.tasks.is_empty() => {
                handle_task_result(result);
            }
        }
    }

    Ok(())
}

/// Remove deferred entries for issues no longer in the ready list and clear
/// their numbers from other deferred issues' awaiting sets.
async fn clean_stale_deferred(
    deferred: &Arc<Mutex<HashMap<u32, DeferredIssue>>>,
    ready_numbers: &HashSet<u32>,
) {
    let mut def_guard = deferred.lock().await;
    let stale: HashSet<u32> =
        def_guard.keys().filter(|num| !ready_numbers.contains(num)).copied().collect();
    if !stale.is_empty() {
        info!(count = stale.len(), "removing stale deferred issues");
        def_guard.retain(|num, _| !stale.contains(num));
        for d in def_guard.values_mut() {
            d.awaiting.retain(|n| !stale.contains(n));
        }
    }
}

/// Promote deferred issues whose awaiting sets have fully cleared.
async fn promote_deferred(
    deferred: &Arc<Mutex<HashMap<u32, DeferredIssue>>>,
) -> Vec<(PipelineIssue, InFlightIssue)> {
    let mut promoted = Vec::new();
    let mut def_guard = deferred.lock().await;
    let ready: Vec<u32> =
        def_guard.iter().filter(|(_, d)| d.awaiting.is_empty()).map(|(num, _)| *num).collect();
    for num in ready {
        if let Some(d) = def_guard.remove(&num) {
            info!(issue = num, "promoting deferred issue (dependencies cleared)");
            promoted.push((d.issue, d.metadata));
        }
    }
    promoted
}

/// Single poll cycle: promote ready deferred issues, plan genuinely new ones, and spawn.
///
/// Only invokes the planner for issues not already tracked in `in_flight` or `deferred`.
/// Deferred issues whose `awaiting` set has cleared are promoted without a planner call.
async fn poll_and_spawn<R: CommandRunner + 'static>(
    executor: &Arc<PipelineExecutor<R>>,
    ready_label: &str,
    sched: &mut SchedulerState,
    auto_merge: bool,
) {
    let ready_issues = match executor.issues.get_ready_issues(ready_label).await {
        Ok(i) => i,
        Err(e) => {
            error!(error = %e, "failed to fetch issues");
            return;
        }
    };

    let ready_numbers: HashSet<u32> = ready_issues.iter().map(|i| i.number).collect();
    clean_stale_deferred(&sched.deferred, &ready_numbers).await;

    // Snapshot in-flight and deferred state, then filter to genuinely new issues
    let (in_flight_numbers, in_flight_snapshot) = {
        let guard = sched.in_flight.lock().await;
        let numbers: HashSet<u32> = guard.keys().copied().collect();
        let snapshot: Vec<InFlightIssue> = guard.values().cloned().collect();
        drop(guard);
        (numbers, snapshot)
    };

    let (deferred_numbers, deferred_snapshot) = {
        let guard = sched.deferred.lock().await;
        let numbers: HashSet<u32> = guard.keys().copied().collect();
        let snapshot: Vec<DeferredIssue> = guard.values().cloned().collect();
        drop(guard);
        (numbers, snapshot)
    };

    let new_issues: Vec<_> = ready_issues
        .into_iter()
        .filter(|i| !in_flight_numbers.contains(&i.number) && !deferred_numbers.contains(&i.number))
        .collect();

    let mut to_spawn = promote_deferred(&sched.deferred).await;

    // Only invoke the planner for genuinely new issues
    if !new_issues.is_empty() {
        info!(count = new_issues.len(), "found new issues to evaluate");

        let mut graph_context: Vec<GraphContextNode> = in_flight_snapshot
            .iter()
            .map(|ifl| GraphContextNode {
                number: ifl.number,
                title: ifl.title.clone(),
                state: crate::db::graph::NodeState::InFlight,
                area: ifl.area.clone(),
                predicted_files: ifl.predicted_files.clone(),
                has_migration: ifl.has_migration,
                depends_on: vec![],
            })
            .collect();
        graph_context.extend(deferred_snapshot.iter().map(|d| GraphContextNode {
            number: d.metadata.number,
            title: d.metadata.title.clone(),
            state: crate::db::graph::NodeState::Pending,
            area: d.metadata.area.clone(),
            predicted_files: d.metadata.predicted_files.clone(),
            has_migration: d.metadata.has_migration,
            depends_on: d.awaiting.iter().copied().collect(),
        }));

        if let Some(plan) = executor.plan_issues(&new_issues, &graph_context).await {
            info!(nodes = plan.nodes.len(), total = plan.total_issues, "planner produced a plan");
            apply_plan(&new_issues, &plan, &in_flight_numbers, &mut to_spawn, &sched.deferred)
                .await;
        } else {
            warn!("planner failed, spawning all new issues immediately");
            for issue in &new_issues {
                to_spawn.push((issue.clone(), InFlightIssue::from_issue(issue)));
            }
        }
    }

    if to_spawn.is_empty() {
        if new_issues.is_empty() {
            info!("no actionable issues, waiting");
        }
        return;
    }

    spawn_issues(to_spawn, executor, sched, auto_merge).await;
}

/// Apply a DAG planner output: layer-1 issues spawn immediately, later layers are deferred.
async fn apply_plan(
    new_issues: &[PipelineIssue],
    plan: &PlannerGraphOutput,
    in_flight_numbers: &HashSet<u32>,
    to_spawn: &mut Vec<(PipelineIssue, InFlightIssue)>,
    deferred: &Arc<Mutex<HashMap<u32, DeferredIssue>>>,
) {
    let (spawn_map, defer_list) = split_graph_plan(plan, in_flight_numbers);
    let issue_map: HashMap<u32, &PipelineIssue> =
        new_issues.iter().map(|i| (i.number, i)).collect();

    for issue in new_issues {
        if let Some(metadata) = spawn_map.get(&issue.number) {
            to_spawn.push((issue.clone(), metadata.clone()));
        }
    }

    let mut def_guard = deferred.lock().await;
    for (number, metadata, awaiting) in defer_list {
        if let Some(issue) = issue_map.get(&number) {
            info!(
                issue = number,
                awaiting_count = awaiting.len(),
                "deferring issue (waiting for dependencies)"
            );
            def_guard.insert(number, DeferredIssue { issue: (*issue).clone(), metadata, awaiting });
        }
    }
}

/// Spawn pipeline tasks for a set of issues.
async fn spawn_issues<R: CommandRunner + 'static>(
    to_spawn: Vec<(PipelineIssue, InFlightIssue)>,
    executor: &Arc<PipelineExecutor<R>>,
    sched: &mut SchedulerState,
    auto_merge: bool,
) {
    for (issue, metadata) in to_spawn {
        let sem = Arc::clone(&sched.semaphore);
        let exec = Arc::clone(executor);
        let in_fl = Arc::clone(&sched.in_flight);
        let def = Arc::clone(&sched.deferred);
        let number = issue.number;
        let complexity = Some(metadata.complexity.clone());

        in_fl.lock().await.insert(number, metadata);

        sched.tasks.spawn(async move {
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    in_fl.lock().await.remove(&number);
                    return (number, Err(anyhow::anyhow!("semaphore closed: {e}")));
                }
            };
            let result = exec.run_issue_pipeline(&issue, auto_merge, complexity).await;
            let outcome = match result {
                Ok(outcome) => {
                    if let Err(e) = exec.finalize_merge(&outcome, &issue).await {
                        warn!(issue = number, error = %e, "finalize_merge failed");
                    }
                    Ok(outcome)
                }
                Err(e) => Err(e),
            };
            in_fl.lock().await.remove(&number);
            // Clear this issue from deferred awaiting sets so dependents can be promoted
            {
                let mut def_guard = def.lock().await;
                for d in def_guard.values_mut() {
                    d.awaiting.remove(&number);
                }
            }
            drop(permit);
            (number, outcome)
        });
    }
}

/// A deferred issue's number, planner metadata, and the set of issues it must wait for.
type DeferredEntry = (u32, InFlightIssue, HashSet<u32>);

/// Separate a DAG planner output into layer 1 (spawn immediately) and deferred layers.
///
/// `in_flight_numbers` are issue numbers currently running -- they are added to the
/// awaiting set of deferred issues (nodes that depend on them must wait).
fn split_graph_plan(
    plan: &PlannerGraphOutput,
    in_flight_numbers: &HashSet<u32>,
) -> (HashMap<u32, InFlightIssue>, Vec<DeferredEntry>) {
    let layers = topological_layers(plan);
    let mut to_spawn = HashMap::new();
    let mut to_defer = Vec::new();
    let mut lower_layers: HashSet<u32> = in_flight_numbers.clone();

    for (idx, layer) in layers.iter().enumerate() {
        if idx == 0 {
            for &node in layer {
                to_spawn.insert(node.number, InFlightIssue::from(node));
            }
        } else {
            for &node in layer {
                to_defer.push((node.number, InFlightIssue::from(node), lower_layers.clone()));
            }
        }
        for node in layer {
            lower_layers.insert(node.number);
        }
    }

    (to_spawn, to_defer)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf};

    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        agents::{Complexity, PlannedNode, PlannerGraphOutput},
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
        use super::PipelineOutcome;
        let outcome = PipelineOutcome {
            run_id: "test-run".to_string(),
            pr_number: 1,
            worktree_path: PathBuf::from("/tmp/wt"),
            target_dir: PathBuf::from("/tmp"),
        };
        handle_task_result(Ok((1, Ok(outcome))));
    }

    #[test]
    fn handle_task_result_does_not_panic_on_error() {
        handle_task_result(Ok((1, Err(anyhow::anyhow!("test error")))));
    }

    fn make_node(number: u32, title: &str, area: &str, complexity: Complexity) -> PlannedNode {
        PlannedNode {
            number,
            title: title.to_string(),
            area: area.to_string(),
            predicted_files: vec![],
            has_migration: false,
            complexity,
            depends_on: vec![],
            reasoning: String::new(),
        }
    }

    #[test]
    fn split_graph_plan_separates_layers() {
        let plan = PlannerGraphOutput {
            nodes: vec![
                make_node(1, "First", "cli", Complexity::Simple),
                make_node(2, "Second", "config", Complexity::Full),
                {
                    let mut n = make_node(3, "Third", "db", Complexity::Full);
                    n.depends_on = vec![1, 2];
                    n.has_migration = true;
                    n
                },
            ],
            total_issues: 3,
            parallel_capacity: 2,
        };

        let (spawn_map, defer_list) = split_graph_plan(&plan, &HashSet::new());

        assert_eq!(spawn_map.len(), 2);
        assert_eq!(spawn_map.get(&1).unwrap().complexity, Complexity::Simple);
        assert_eq!(spawn_map.get(&1).unwrap().area, "cli");
        assert_eq!(spawn_map.get(&2).unwrap().complexity, Complexity::Full);

        assert_eq!(defer_list.len(), 1);
        let (num, meta, awaiting) = &defer_list[0];
        assert_eq!(*num, 3);
        assert_eq!(meta.area, "db");
        assert!(awaiting.contains(&1));
        assert!(awaiting.contains(&2));
        assert_eq!(awaiting.len(), 2);
    }

    #[test]
    fn split_graph_plan_empty() {
        let plan = PlannerGraphOutput { nodes: vec![], total_issues: 0, parallel_capacity: 0 };
        let (spawn_map, defer_list) = split_graph_plan(&plan, &HashSet::new());
        assert!(spawn_map.is_empty());
        assert!(defer_list.is_empty());
    }

    #[test]
    fn split_graph_plan_includes_in_flight_in_awaiting() {
        let plan = PlannerGraphOutput {
            nodes: vec![make_node(5, "New", "cli", Complexity::Simple), {
                let mut n = make_node(6, "Depends", "db", Complexity::Full);
                n.depends_on = vec![5];
                n.has_migration = true;
                n
            }],
            total_issues: 2,
            parallel_capacity: 1,
        };

        let in_flight_nums: HashSet<u32> = [10, 11].into_iter().collect();
        let (spawn_map, defer_list) = split_graph_plan(&plan, &in_flight_nums);

        assert_eq!(spawn_map.len(), 1);
        assert!(spawn_map.contains_key(&5));

        assert_eq!(defer_list.len(), 1);
        let (num, _, awaiting) = &defer_list[0];
        assert_eq!(*num, 6);
        assert!(awaiting.contains(&10));
        assert!(awaiting.contains(&11));
        assert!(awaiting.contains(&5));
        assert_eq!(awaiting.len(), 3);
    }

    #[test]
    fn split_graph_plan_three_layers_chain_awaiting() {
        let plan = PlannerGraphOutput {
            nodes: vec![
                make_node(1, "A", "a", Complexity::Simple),
                {
                    let mut n = make_node(2, "B", "b", Complexity::Full);
                    n.depends_on = vec![1];
                    n
                },
                {
                    let mut n = make_node(3, "C", "c", Complexity::Full);
                    n.depends_on = vec![2];
                    n
                },
            ],
            total_issues: 3,
            parallel_capacity: 1,
        };

        let (spawn_map, defer_list) = split_graph_plan(&plan, &HashSet::new());

        assert_eq!(spawn_map.len(), 1);
        assert!(spawn_map.contains_key(&1));

        assert_eq!(defer_list.len(), 2);
        let (_, _, awaiting_2) = &defer_list[0];
        assert_eq!(*awaiting_2, HashSet::from([1]));
        let (_, _, awaiting_3) = &defer_list[1];
        assert_eq!(*awaiting_3, HashSet::from([1, 2]));
    }

    #[tokio::test]
    async fn deferred_issues_filtered_from_new_issues() {
        let in_flight: Arc<Mutex<HashMap<u32, InFlightIssue>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let deferred: Arc<Mutex<HashMap<u32, DeferredIssue>>> =
            Arc::new(Mutex::new(HashMap::new()));

        in_flight.lock().await.insert(
            1,
            InFlightIssue {
                number: 1,
                title: "Running".to_string(),
                area: "auth".to_string(),
                predicted_files: vec![],
                has_migration: false,
                complexity: Complexity::Full,
            },
        );

        deferred.lock().await.insert(
            2,
            DeferredIssue {
                issue: PipelineIssue {
                    number: 2,
                    title: "Waiting".to_string(),
                    body: String::new(),
                    source: IssueOrigin::Github,
                    target_repo: None,
                },
                metadata: InFlightIssue {
                    number: 2,
                    title: "Waiting".to_string(),
                    area: "db".to_string(),
                    predicted_files: vec![],
                    has_migration: false,
                    complexity: Complexity::Full,
                },
                awaiting: HashSet::from([1]),
            },
        );

        let issues = vec![
            PipelineIssue {
                number: 1,
                title: "Running".to_string(),
                body: String::new(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
            PipelineIssue {
                number: 2,
                title: "Waiting".to_string(),
                body: String::new(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
            PipelineIssue {
                number: 3,
                title: "New".to_string(),
                body: String::new(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
        ];

        let ifl = in_flight.lock().await;
        let def = deferred.lock().await;
        let new_issues: Vec<_> = issues
            .into_iter()
            .filter(|i| !ifl.contains_key(&i.number) && !def.contains_key(&i.number))
            .collect();
        drop(ifl);
        drop(def);

        assert_eq!(new_issues.len(), 1);
        assert_eq!(new_issues[0].number, 3);
    }

    #[tokio::test]
    async fn deferred_promotion_when_awaiting_clears() {
        let deferred: Arc<Mutex<HashMap<u32, DeferredIssue>>> =
            Arc::new(Mutex::new(HashMap::new()));

        deferred.lock().await.insert(
            3,
            DeferredIssue {
                issue: PipelineIssue {
                    number: 3,
                    title: "Deferred".to_string(),
                    body: String::new(),
                    source: IssueOrigin::Github,
                    target_repo: None,
                },
                metadata: InFlightIssue {
                    number: 3,
                    title: "Deferred".to_string(),
                    area: "db".to_string(),
                    predicted_files: vec![],
                    has_migration: true,
                    complexity: Complexity::Full,
                },
                awaiting: HashSet::from([1, 2]),
            },
        );

        // Issue 1 completes
        {
            let mut guard = deferred.lock().await;
            for d in guard.values_mut() {
                d.awaiting.remove(&1);
            }
        }

        // Still waiting on issue 2
        assert!(
            deferred.lock().await.values().all(|d| !d.awaiting.is_empty()),
            "should not be promotable yet"
        );

        // Issue 2 completes
        {
            let mut guard = deferred.lock().await;
            for d in guard.values_mut() {
                d.awaiting.remove(&2);
            }
        }

        // Now issue 3 is promotable
        {
            let guard = deferred.lock().await;
            let promotable: Vec<u32> =
                guard.iter().filter(|(_, d)| d.awaiting.is_empty()).map(|(n, _)| *n).collect();
            assert_eq!(promotable, vec![3]);
            drop(guard);
        }
    }

    #[tokio::test]
    async fn stale_deferred_issues_cleaned_up() {
        let deferred: Arc<Mutex<HashMap<u32, DeferredIssue>>> =
            Arc::new(Mutex::new(HashMap::new()));

        {
            let mut guard = deferred.lock().await;
            guard.insert(
                2,
                DeferredIssue {
                    issue: PipelineIssue {
                        number: 2,
                        title: "Two".to_string(),
                        body: String::new(),
                        source: IssueOrigin::Github,
                        target_repo: None,
                    },
                    metadata: InFlightIssue {
                        number: 2,
                        title: "Two".to_string(),
                        area: "a".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                    },
                    awaiting: HashSet::from([1]),
                },
            );
            guard.insert(
                3,
                DeferredIssue {
                    issue: PipelineIssue {
                        number: 3,
                        title: "Three".to_string(),
                        body: String::new(),
                        source: IssueOrigin::Github,
                        target_repo: None,
                    },
                    metadata: InFlightIssue {
                        number: 3,
                        title: "Three".to_string(),
                        area: "b".to_string(),
                        predicted_files: vec![],
                        has_migration: false,
                        complexity: Complexity::Full,
                    },
                    awaiting: HashSet::from([1, 2]),
                },
            );
        }

        // Issue 2 no longer in ready list (closed externally)
        let ready_numbers: HashSet<u32> = HashSet::from([3]);
        clean_stale_deferred(&deferred, &ready_numbers).await;

        let guard = deferred.lock().await;
        assert!(!guard.contains_key(&2));
        let d3 = guard.get(&3).unwrap();
        let has_2 = d3.awaiting.contains(&2);
        let has_1 = d3.awaiting.contains(&1);
        drop(guard);
        assert!(!has_2);
        assert!(has_1);
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
