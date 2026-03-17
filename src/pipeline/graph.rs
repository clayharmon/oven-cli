use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::{info, warn};

use crate::{
    agents::{PlannedNode, PlannerGraphOutput},
    db::graph::{self, GraphNodeRow, NodeState},
    issues::PipelineIssue,
};

/// A node in the dependency graph, holding all metadata for scheduling.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub issue_number: u32,
    pub title: String,
    pub area: String,
    pub predicted_files: Vec<String>,
    pub has_migration: bool,
    pub complexity: String,
    pub state: NodeState,
    pub pr_number: Option<u32>,
    pub run_id: Option<String>,
    pub issue: Option<PipelineIssue>,
    /// Target repo name for multi-repo routing. Persisted separately from `issue`
    /// so it survives DB round-trips (where `issue` is `None`).
    pub target_repo: Option<String>,
}

/// Directed acyclic graph tracking issue dependencies.
///
/// Edges point from dependent to dependency: if A depends on B, `edges[A]`
/// contains B. The graph enforces acyclicity on insertion.
pub struct DependencyGraph {
    session_id: String,
    nodes: HashMap<u32, GraphNode>,
    /// Forward edges: `edges[a]` = set of issues that `a` depends on.
    edges: HashMap<u32, HashSet<u32>>,
    /// Reverse edges: `reverse_edges[b]` = set of issues that depend on `b`.
    reverse_edges: HashMap<u32, HashSet<u32>>,
}

impl DependencyGraph {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            reverse_edges: HashMap::new(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn contains(&self, issue: u32) -> bool {
        self.nodes.contains_key(&issue)
    }

    pub fn node(&self, issue: u32) -> Option<&GraphNode> {
        self.nodes.get(&issue)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn add_node(&mut self, node: GraphNode) {
        let num = node.issue_number;
        self.nodes.insert(num, node);
        self.edges.entry(num).or_default();
        self.reverse_edges.entry(num).or_default();
    }

    /// Add a dependency edge: `from` depends on `to`.
    ///
    /// Returns `false` if the edge would create a cycle (edge not added).
    pub fn add_edge(&mut self, from: u32, to: u32) -> bool {
        if from == to || self.would_create_cycle(from, to) {
            return false;
        }
        self.edges.entry(from).or_default().insert(to);
        self.reverse_edges.entry(to).or_default().insert(from);
        true
    }

    /// Check if adding an edge from -> to would create a cycle.
    ///
    /// A cycle exists if `to` transitively depends on `from`.
    pub fn would_create_cycle(&self, from: u32, to: u32) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![to];
        while let Some(current) = stack.pop() {
            if current == from {
                return true;
            }
            if visited.insert(current) {
                if let Some(deps) = self.edges.get(&current) {
                    for &dep in deps {
                        if !visited.contains(&dep) {
                            stack.push(dep);
                        }
                    }
                }
            }
        }
        false
    }

    /// Issues in `Pending` state whose dependencies are all `Merged`.
    pub fn ready_issues(&self) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.state == NodeState::Pending)
            .filter(|(num, _)| {
                self.edges.get(num).is_none_or(|deps| {
                    deps.iter()
                        .all(|d| self.nodes.get(d).is_some_and(|n| n.state == NodeState::Merged))
                })
            })
            .map(|(num, _)| *num)
            .collect()
    }

    /// Issues currently in `AwaitingMerge` state.
    pub fn awaiting_merge(&self) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.state == NodeState::AwaitingMerge)
            .map(|(num, _)| *num)
            .collect()
    }

    /// Transition a node to a new state.
    pub fn transition(&mut self, issue: u32, state: NodeState) {
        if let Some(node) = self.nodes.get_mut(&issue) {
            info!(
                issue,
                from = %node.state,
                to = %state,
                "graph node state transition"
            );
            node.state = state;
        }
    }

    /// Set the PR number on a node.
    pub fn set_pr_number(&mut self, issue: u32, pr_number: u32) {
        if let Some(node) = self.nodes.get_mut(&issue) {
            node.pr_number = Some(pr_number);
        }
    }

    /// Set the run ID on a node.
    pub fn set_run_id(&mut self, issue: u32, run_id: &str) {
        if let Some(node) = self.nodes.get_mut(&issue) {
            node.run_id = Some(run_id.to_string());
        }
    }

    /// Get the set of issues that `issue` depends on.
    pub fn dependencies(&self, issue: u32) -> HashSet<u32> {
        self.edges.get(&issue).cloned().unwrap_or_default()
    }

    /// Get the set of issues that depend on `issue`.
    pub fn dependents(&self, issue: u32) -> HashSet<u32> {
        self.reverse_edges.get(&issue).cloned().unwrap_or_default()
    }

    /// Whether every node is in a terminal state (`Merged` or `Failed`).
    pub fn all_terminal(&self) -> bool {
        self.nodes.values().all(|n| matches!(n.state, NodeState::Merged | NodeState::Failed))
    }

    /// Whether a node is blocked because at least one dependency has failed.
    pub fn is_blocked(&self, issue: u32) -> bool {
        self.edges.get(&issue).is_some_and(|deps| {
            deps.iter().any(|d| self.nodes.get(d).is_some_and(|n| n.state == NodeState::Failed))
        })
    }

    /// Propagate failure from a node to all transitive dependents.
    ///
    /// Any `Pending` or `InFlight` node reachable via `reverse_edges` from the
    /// failed node is transitioned to `Failed`. Returns the list of issue
    /// numbers that were newly failed (excludes the original node).
    pub fn propagate_failure(&mut self, issue: u32) -> Vec<u32> {
        use std::collections::VecDeque;

        let mut queue = VecDeque::new();
        let mut newly_failed = Vec::new();

        // Seed with direct dependents of the failed node
        if let Some(dependents) = self.reverse_edges.get(&issue) {
            queue.extend(dependents.iter().copied());
        }

        let mut visited = HashSet::new();
        visited.insert(issue);

        while let Some(current) = queue.pop_front() {
            if !visited.insert(current) {
                continue;
            }
            let dominated = self
                .nodes
                .get(&current)
                .is_some_and(|n| matches!(n.state, NodeState::Pending | NodeState::InFlight));
            if !dominated {
                continue;
            }
            self.transition(current, NodeState::Failed);
            newly_failed.push(current);
            if let Some(dependents) = self.reverse_edges.get(&current) {
                queue.extend(dependents.iter().copied());
            }
        }

        newly_failed
    }

    /// Remove a node and all its edges (for stale issue cleanup).
    pub fn remove_node(&mut self, issue: u32) {
        self.nodes.remove(&issue);
        if let Some(deps) = self.edges.remove(&issue) {
            for dep in &deps {
                if let Some(rev) = self.reverse_edges.get_mut(dep) {
                    rev.remove(&issue);
                }
            }
        }
        if let Some(dependents) = self.reverse_edges.remove(&issue) {
            for dependent in &dependents {
                if let Some(fwd) = self.edges.get_mut(dependent) {
                    fwd.remove(&issue);
                }
            }
        }
    }

    /// All issue numbers in the graph.
    pub fn all_issues(&self) -> Vec<u32> {
        let mut nums: Vec<u32> = self.nodes.keys().copied().collect();
        nums.sort_unstable();
        nums
    }

    /// Load graph state from the database.
    pub fn from_db(conn: &Connection, session_id: &str) -> Result<Self> {
        let db_nodes = graph::get_nodes(conn, session_id).context("loading graph nodes")?;
        let db_edges = graph::get_edges(conn, session_id).context("loading graph edges")?;

        let mut g = Self::new(session_id);
        for row in &db_nodes {
            g.add_node(GraphNode {
                issue_number: row.issue_number,
                title: row.title.clone(),
                area: row.area.clone(),
                predicted_files: row.predicted_files.clone(),
                has_migration: row.has_migration,
                complexity: row.complexity.clone(),
                state: row.state,
                pr_number: row.pr_number,
                run_id: row.run_id.clone(),
                issue: None,
                target_repo: row.target_repo.clone(),
            });
        }
        for (from, to) in &db_edges {
            if !g.add_edge(*from, *to) {
                warn!(from, to, "skipping persisted edge that would create cycle");
            }
        }

        Ok(g)
    }

    /// Persist the full graph state to the database, replacing any existing data for
    /// this session. Runs inside a transaction so a crash mid-save cannot leave a
    /// partial graph.
    pub fn save_to_db(&self, conn: &Connection) -> Result<()> {
        let tx = conn.unchecked_transaction().context("starting graph save transaction")?;

        graph::delete_session(&tx, &self.session_id)?;
        for node in self.nodes.values() {
            let row = GraphNodeRow {
                issue_number: node.issue_number,
                session_id: self.session_id.clone(),
                state: node.state,
                pr_number: node.pr_number,
                run_id: node.run_id.clone(),
                title: node.title.clone(),
                area: node.area.clone(),
                predicted_files: node.predicted_files.clone(),
                has_migration: node.has_migration,
                complexity: node.complexity.clone(),
                target_repo: node.target_repo.clone(),
            };
            graph::insert_node(&tx, &self.session_id, &row)?;
        }
        for (&from, deps) in &self.edges {
            for &to in deps {
                graph::insert_edge(&tx, &self.session_id, from, to)?;
            }
        }

        tx.commit().context("committing graph save transaction")?;
        Ok(())
    }

    /// Build a graph from planner output, matching issues by number.
    pub fn from_planner_output(
        session_id: &str,
        plan: &PlannerGraphOutput,
        issues: &[PipelineIssue],
    ) -> Self {
        let issue_map: HashMap<u32, &PipelineIssue> =
            issues.iter().map(|i| (i.number, i)).collect();
        let mut g = Self::new(session_id);
        for node in &plan.nodes {
            g.add_node(node_from_planned(node, issue_map.get(&node.number).copied()));
        }
        add_planned_edges(&mut g, &plan.nodes);
        g
    }

    /// Merge new planner output into an existing graph (polling mode).
    ///
    /// Only adds nodes not already present. Edges between new nodes and
    /// existing nodes are added if they don't create cycles.
    pub fn merge_planner_output(&mut self, plan: &PlannerGraphOutput, issues: &[PipelineIssue]) {
        let issue_map: HashMap<u32, &PipelineIssue> =
            issues.iter().map(|i| (i.number, i)).collect();
        let new_nodes: Vec<&PlannedNode> =
            plan.nodes.iter().filter(|n| !self.contains(n.number)).collect();
        for node in &new_nodes {
            self.add_node(node_from_planned(node, issue_map.get(&node.number).copied()));
        }
        add_planned_edges(self, &new_nodes);
    }

    /// Format the graph for display in CLI output.
    pub fn display_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let issues = self.all_issues();

        for num in issues {
            let Some(node) = self.nodes.get(&num) else { continue };
            let blocked = if self.is_blocked(num) { " (blocked)" } else { "" };
            let state_str = format!("[{}]{blocked}", node.state);
            lines.push(format!("  #{num} {} {:.<40} {state_str}", node.title, "."));
            let deps = self.dependencies(num);
            if !deps.is_empty() {
                let mut dep_nums: Vec<u32> = deps.into_iter().collect();
                dep_nums.sort_unstable();
                let dep_strs: Vec<String> = dep_nums.iter().map(|d| format!("#{d}")).collect();
                lines.push(format!("    depends on: {}", dep_strs.join(", ")));
            }
        }
        lines
    }

    /// Build planner context from the current graph state.
    ///
    /// Produces one `GraphContextNode` per node so the planner can see
    /// in-flight work and avoid scheduling conflicts.
    pub fn to_graph_context(&self) -> Vec<crate::agents::GraphContextNode> {
        self.all_issues()
            .into_iter()
            .filter_map(|num| {
                let node = self.nodes.get(&num)?;
                let depends_on: Vec<u32> = self.edges.get(&num).map_or_else(Vec::new, |deps| {
                    let mut v: Vec<u32> = deps.iter().copied().collect();
                    v.sort_unstable();
                    v
                });
                Some(crate::agents::GraphContextNode {
                    number: num,
                    title: node.title.clone(),
                    state: node.state,
                    area: node.area.clone(),
                    predicted_files: node.predicted_files.clone(),
                    has_migration: node.has_migration,
                    depends_on,
                    target_repo: node.target_repo.clone(),
                })
            })
            .collect()
    }
}

fn node_from_planned(node: &PlannedNode, issue: Option<&PipelineIssue>) -> GraphNode {
    GraphNode {
        issue_number: node.number,
        title: node.title.clone(),
        area: node.area.clone(),
        predicted_files: node.predicted_files.clone(),
        has_migration: node.has_migration,
        complexity: node.complexity.to_string(),
        state: NodeState::Pending,
        pr_number: None,
        run_id: None,
        target_repo: issue.and_then(|i| i.target_repo.clone()),
        issue: issue.cloned(),
    }
}

fn add_planned_edges(graph: &mut DependencyGraph, nodes: &[impl std::borrow::Borrow<PlannedNode>]) {
    for node in nodes {
        let node = node.borrow();
        for &dep in &node.depends_on {
            if !graph.add_edge(node.number, dep) {
                warn!(
                    from = node.number,
                    to = dep,
                    "skipping planner edge that would create cycle"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(num: u32) -> GraphNode {
        GraphNode {
            issue_number: num,
            title: format!("Issue #{num}"),
            area: "test".to_string(),
            predicted_files: vec![],
            has_migration: false,
            complexity: "full".to_string(),
            state: NodeState::Pending,
            pr_number: None,
            run_id: None,
            issue: None,
            target_repo: None,
        }
    }

    #[test]
    fn add_node_and_check() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        assert!(g.contains(1));
        assert!(!g.contains(2));
        assert_eq!(g.node_count(), 1);
    }

    #[test]
    fn add_edge_and_check() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        assert!(g.add_edge(2, 1)); // 2 depends on 1

        assert_eq!(g.dependencies(2), HashSet::from([1]));
        assert_eq!(g.dependents(1), HashSet::from([2]));
    }

    #[test]
    fn self_edge_rejected() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        assert!(!g.add_edge(1, 1));
    }

    #[test]
    fn direct_cycle_detected() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        assert!(g.add_edge(2, 1)); // 2 depends on 1
        assert!(!g.add_edge(1, 2)); // would create cycle
    }

    #[test]
    fn indirect_cycle_detected() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        assert!(g.add_edge(2, 1)); // 2 depends on 1
        assert!(g.add_edge(3, 2)); // 3 depends on 2
        assert!(!g.add_edge(1, 3)); // would create 1 -> 3 -> 2 -> 1 cycle
    }

    #[test]
    fn valid_dag_no_false_cycle() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        assert!(g.add_edge(2, 1));
        assert!(g.add_edge(3, 1)); // diamond top, both depend on 1
        assert!(g.add_edge(3, 2)); // 3 also depends on 2
    }

    #[test]
    fn ready_issues_returns_pending_with_merged_deps() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_edge(2, 1);

        // 1 is pending with no deps, so it's ready
        assert_eq!(g.ready_issues(), vec![1]);

        // Merge node 1, now node 2 should be ready
        g.transition(1, NodeState::Merged);
        let ready = g.ready_issues();
        assert_eq!(ready, vec![2]);
    }

    #[test]
    fn ready_issues_empty_when_deps_in_flight() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_edge(2, 1);
        g.transition(1, NodeState::InFlight);
        assert!(g.ready_issues().is_empty());
    }

    #[test]
    fn ready_issues_empty_when_deps_awaiting_merge() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_edge(2, 1);
        g.transition(1, NodeState::AwaitingMerge);
        assert!(g.ready_issues().is_empty());
    }

    #[test]
    fn awaiting_merge_returns_correct_nodes() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.transition(1, NodeState::AwaitingMerge);
        let awaiting = g.awaiting_merge();
        assert_eq!(awaiting, vec![1]);
    }

    #[test]
    fn all_terminal_checks_all_nodes() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        assert!(!g.all_terminal());

        g.transition(1, NodeState::Merged);
        assert!(!g.all_terminal());

        g.transition(2, NodeState::Failed);
        assert!(g.all_terminal());
    }

    #[test]
    fn is_blocked_when_dep_failed() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_edge(2, 1);
        g.transition(1, NodeState::Failed);
        assert!(g.is_blocked(2));
        assert!(!g.is_blocked(1));
    }

    #[test]
    fn remove_node_cleans_edges() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 2);

        g.remove_node(2);
        assert!(!g.contains(2));
        // Edge from 3 to 2 should be gone
        assert!(g.dependencies(3).is_empty());
        // Reverse edge from 1 to 2 should be gone
        assert!(g.dependents(1).is_empty());
    }

    #[test]
    fn display_lines_format() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_edge(2, 1);
        g.transition(1, NodeState::Merged);

        let lines = g.display_lines();
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l.contains("#1")));
        assert!(lines.iter().any(|l| l.contains("depends on")));
    }

    #[test]
    fn db_roundtrip() {
        let conn = crate::db::open_in_memory().unwrap();
        let mut g = DependencyGraph::new("test-session");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 1);
        g.add_edge(3, 2);
        g.transition(1, NodeState::Merged);
        g.set_pr_number(1, 99);
        g.set_run_id(1, "abc");

        g.save_to_db(&conn).unwrap();

        let loaded = DependencyGraph::from_db(&conn, "test-session").unwrap();
        assert_eq!(loaded.node_count(), 3);
        assert_eq!(loaded.dependencies(2), HashSet::from([1]));
        assert_eq!(loaded.dependencies(3), HashSet::from([1, 2]));
        assert_eq!(loaded.node(1).unwrap().state, NodeState::Merged);
        assert_eq!(loaded.node(1).unwrap().pr_number, Some(99));
        assert_eq!(loaded.node(1).unwrap().run_id.as_deref(), Some("abc"));
    }

    #[test]
    fn diamond_graph_ready_ordering() {
        // A -> B, A -> C, B -> D, C -> D (D is the root)
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1)); // D
        g.add_node(make_node(2)); // B
        g.add_node(make_node(3)); // C
        g.add_node(make_node(4)); // A

        g.add_edge(2, 1); // B depends on D
        g.add_edge(3, 1); // C depends on D
        g.add_edge(4, 2); // A depends on B
        g.add_edge(4, 3); // A depends on C

        // Only D is ready initially
        assert_eq!(g.ready_issues(), vec![1]);

        // Merge D, B and C become ready
        g.transition(1, NodeState::Merged);
        let mut ready = g.ready_issues();
        ready.sort_unstable();
        assert_eq!(ready, vec![2, 3]);

        // Merge B, A still waiting on C
        g.transition(2, NodeState::Merged);
        assert_eq!(g.ready_issues(), vec![3]);

        // Merge C, now A is ready
        g.transition(3, NodeState::Merged);
        assert_eq!(g.ready_issues(), vec![4]);
    }

    #[test]
    fn empty_graph_is_all_terminal() {
        let g = DependencyGraph::new("test");
        assert!(g.all_terminal());
    }

    #[test]
    fn independent_nodes_all_ready() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));

        let mut ready = g.ready_issues();
        ready.sort_unstable();
        assert_eq!(ready, vec![1, 2, 3]);
    }

    fn make_planned(number: u32, depends_on: Vec<u32>) -> crate::agents::PlannedNode {
        crate::agents::PlannedNode {
            number,
            title: format!("Issue #{number}"),
            area: "test".to_string(),
            predicted_files: vec![],
            has_migration: false,
            complexity: crate::agents::Complexity::Full,
            depends_on,
            reasoning: String::new(),
        }
    }

    fn make_issue(number: u32) -> PipelineIssue {
        PipelineIssue {
            number,
            title: format!("Issue #{number}"),
            body: String::new(),
            source: crate::issues::IssueOrigin::Github,
            target_repo: None,
            author: None,
        }
    }

    #[test]
    fn from_planner_output_basic() {
        let plan = crate::agents::PlannerGraphOutput {
            nodes: vec![
                make_planned(1, vec![]),
                make_planned(2, vec![]),
                make_planned(3, vec![1, 2]),
            ],
            total_issues: 3,
            parallel_capacity: 2,
        };
        let issues = vec![make_issue(1), make_issue(2), make_issue(3)];

        let g = DependencyGraph::from_planner_output("sess", &plan, &issues);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.dependencies(3), HashSet::from([1, 2]));
        assert!(g.dependencies(1).is_empty());
        // Issues should be attached
        assert!(g.node(1).unwrap().issue.is_some());
        assert!(g.node(2).unwrap().issue.is_some());
    }

    #[test]
    fn from_planner_output_skips_cycle() {
        let plan = crate::agents::PlannerGraphOutput {
            nodes: vec![make_planned(1, vec![2]), make_planned(2, vec![1])],
            total_issues: 2,
            parallel_capacity: 1,
        };

        let g = DependencyGraph::from_planner_output("sess", &plan, &[]);
        // One edge should succeed, the other should be skipped (cycle)
        assert_eq!(g.node_count(), 2);
        let total_edges: usize = [1, 2].iter().map(|n| g.dependencies(*n).len()).sum();
        assert_eq!(total_edges, 1);
    }

    #[test]
    fn merge_planner_output_adds_new_only() {
        let plan1 = crate::agents::PlannerGraphOutput {
            nodes: vec![make_planned(1, vec![])],
            total_issues: 1,
            parallel_capacity: 1,
        };
        let mut g = DependencyGraph::from_planner_output("sess", &plan1, &[make_issue(1)]);
        g.transition(1, NodeState::InFlight);

        // Merge a plan that includes node 1 again and adds node 2
        let plan2 = crate::agents::PlannerGraphOutput {
            nodes: vec![make_planned(1, vec![]), make_planned(2, vec![1])],
            total_issues: 2,
            parallel_capacity: 1,
        };
        g.merge_planner_output(&plan2, &[make_issue(2)]);

        assert_eq!(g.node_count(), 2);
        // Node 1 should still be InFlight (not overwritten)
        assert_eq!(g.node(1).unwrap().state, NodeState::InFlight);
        // Node 2 should be Pending with edge to 1
        assert_eq!(g.node(2).unwrap().state, NodeState::Pending);
        assert_eq!(g.dependencies(2), HashSet::from([1]));
    }

    #[test]
    fn merge_planner_output_cross_edges() {
        let mut g = DependencyGraph::new("sess");
        g.add_node(make_node(1));
        g.transition(1, NodeState::Merged);

        let plan = crate::agents::PlannerGraphOutput {
            nodes: vec![make_planned(2, vec![1])],
            total_issues: 1,
            parallel_capacity: 1,
        };
        g.merge_planner_output(&plan, &[make_issue(2)]);

        assert_eq!(g.dependencies(2), HashSet::from([1]));
        // Node 2 should be ready since node 1 is merged
        assert_eq!(g.ready_issues(), vec![2]);
    }

    #[test]
    fn propagate_failure_linear_chain() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 2);

        g.transition(1, NodeState::Failed);
        let mut failed = g.propagate_failure(1);
        failed.sort_unstable();
        assert_eq!(failed, vec![2, 3]);
        assert_eq!(g.node(2).unwrap().state, NodeState::Failed);
        assert_eq!(g.node(3).unwrap().state, NodeState::Failed);
    }

    #[test]
    fn propagate_failure_diamond() {
        // 1 is root, 2 and 3 depend on 1, 4 depends on 2 and 3
        let mut g = DependencyGraph::new("test");
        for i in 1..=4 {
            g.add_node(make_node(i));
        }
        g.add_edge(2, 1);
        g.add_edge(3, 1);
        g.add_edge(4, 2);
        g.add_edge(4, 3);

        g.transition(1, NodeState::Failed);
        let mut failed = g.propagate_failure(1);
        failed.sort_unstable();
        assert_eq!(failed, vec![2, 3, 4]);
    }

    #[test]
    fn propagate_failure_partial_branch() {
        // 1 and 2 are roots, 3 depends on 1, 4 depends on 2
        let mut g = DependencyGraph::new("test");
        for i in 1..=4 {
            g.add_node(make_node(i));
        }
        g.add_edge(3, 1);
        g.add_edge(4, 2);

        g.transition(1, NodeState::Failed);
        let failed = g.propagate_failure(1);
        assert_eq!(failed, vec![3]);
        // Node 4 should still be Pending (unrelated branch)
        assert_eq!(g.node(4).unwrap().state, NodeState::Pending);
    }

    #[test]
    fn propagate_failure_skips_merged() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 2);
        // Node 2 already merged before 1 fails (unusual but possible)
        g.transition(2, NodeState::Merged);

        g.transition(1, NodeState::Failed);
        let failed = g.propagate_failure(1);
        // Node 2 is merged, skip. Node 3 depends on 2 (merged), not directly on 1.
        assert!(failed.is_empty());
        assert_eq!(g.node(2).unwrap().state, NodeState::Merged);
        assert_eq!(g.node(3).unwrap().state, NodeState::Pending);
    }

    #[test]
    fn propagate_failure_returns_newly_failed() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 1);

        g.transition(1, NodeState::Failed);
        let mut failed = g.propagate_failure(1);
        failed.sort_unstable();
        assert_eq!(failed, vec![2, 3]);
        // Calling again should return empty (already failed)
        let failed2 = g.propagate_failure(1);
        assert!(failed2.is_empty());
    }

    #[test]
    fn to_graph_context_includes_all_nodes() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 1);
        g.add_edge(3, 2);
        g.transition(1, NodeState::InFlight);

        let ctx = g.to_graph_context();
        assert_eq!(ctx.len(), 3);

        let ctx_map: HashMap<u32, &crate::agents::GraphContextNode> =
            ctx.iter().map(|c| (c.number, c)).collect();

        let c1 = ctx_map[&1];
        assert_eq!(c1.state, NodeState::InFlight);
        assert!(c1.depends_on.is_empty());

        let c2 = ctx_map[&2];
        assert_eq!(c2.state, NodeState::Pending);
        assert_eq!(c2.depends_on, vec![1]);

        let c3 = ctx_map[&3];
        assert_eq!(c3.state, NodeState::Pending);
        assert_eq!(c3.depends_on, vec![1, 2]);
    }

    #[test]
    fn save_to_db_is_atomic_on_success() {
        let conn = crate::db::open_in_memory().unwrap();
        let mut g = DependencyGraph::new("atomic-test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_edge(2, 1);

        g.save_to_db(&conn).unwrap();

        // Overwrite with a different graph to verify the delete+insert is atomic
        let mut g2 = DependencyGraph::new("atomic-test");
        g2.add_node(make_node(10));
        g2.save_to_db(&conn).unwrap();

        let loaded = DependencyGraph::from_db(&conn, "atomic-test").unwrap();
        // Old nodes should be gone, only node 10 remains
        assert_eq!(loaded.node_count(), 1);
        assert!(loaded.contains(10));
        assert!(!loaded.contains(1));
        assert!(!loaded.contains(2));
    }
}
