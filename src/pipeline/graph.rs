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

    /// Compute topological layers: layer 0 has no deps, layer N has all deps in layers < N.
    fn compute_layers(&self) -> Vec<Vec<u32>> {
        fn compute_depth(
            node: u32,
            edges: &HashMap<u32, HashSet<u32>>,
            depth: &mut HashMap<u32, usize>,
        ) -> usize {
            if let Some(&d) = depth.get(&node) {
                return d;
            }
            let d = edges.get(&node).map_or(0, |deps| {
                deps.iter().map(|&dep| compute_depth(dep, edges, depth) + 1).max().unwrap_or(0)
            });
            depth.insert(node, d);
            d
        }

        let mut depth: HashMap<u32, usize> = HashMap::new();
        let issues = self.all_issues();

        for &num in &issues {
            compute_depth(num, &self.edges, &mut depth);
        }

        let max_depth = depth.values().copied().max().unwrap_or(0);
        let mut layers: Vec<Vec<u32>> = vec![vec![]; max_depth + 1];
        for &num in &issues {
            let d = depth[&num];
            layers[d].push(num);
        }
        for layer in &mut layers {
            layer.sort_unstable();
        }
        layers
    }

    /// Render a layered box-drawing DAG visualization for terminal output.
    pub fn display_layered(&self) -> Vec<String> {
        if self.nodes.is_empty() {
            return vec!["  (empty graph)".to_string()];
        }

        let layers = self.compute_layers();
        let mut lines = Vec::new();

        // Summary line
        let total = self.nodes.len();
        let merged = self.nodes.values().filter(|n| n.state == NodeState::Merged).count();
        let in_flight = self.nodes.values().filter(|n| n.state == NodeState::InFlight).count();
        let failed = self.nodes.values().filter(|n| n.state == NodeState::Failed).count();

        let mut summary_parts = vec![format!("{total} issues")];
        if merged > 0 {
            summary_parts.push(format!("{merged} merged"));
        }
        if in_flight > 0 {
            summary_parts.push(format!("{in_flight} in flight"));
        }
        if failed > 0 {
            summary_parts.push(format!("{failed} failed"));
        }
        lines.push(format!("  {}", summary_parts.join(", ")));
        lines.push(String::new());

        let box_width = self.compute_box_width();
        let full_box = box_width + 2;
        let spacing = 2;
        let max_per_row = (terminal_width().saturating_sub(4) / (full_box + spacing)).max(1);

        for (layer_idx, layer) in layers.iter().enumerate() {
            // Layer header
            let label = if layer_idx == 0 { "no deps" } else { "" };
            if label.is_empty() {
                lines.push(format!("  Layer {layer_idx}"));
            } else {
                lines.push(format!("  Layer {layer_idx} ({label})"));
            }
            lines.push(String::new());

            // Render boxes for this layer, wrapping into rows
            let boxes: Vec<Vec<String>> =
                layer.iter().map(|&num| self.render_box(num, box_width)).collect();

            for chunk in boxes.chunks(max_per_row) {
                let max_height = chunk.iter().map(Vec::len).max().unwrap_or(0);
                for row in 0..max_height {
                    let mut line = String::from("  ");
                    for (i, b) in chunk.iter().enumerate() {
                        if i > 0 {
                            line.push_str("  ");
                        }
                        if row < b.len() {
                            line.push_str(&b[row]);
                        } else {
                            line.push_str(&" ".repeat(full_box));
                        }
                    }
                    lines.push(line);
                }
            }

            // Draw connectors to next layer if there is one
            if layer_idx + 1 < layers.len() {
                let next_layer = &layers[layer_idx + 1];
                let connector_lines =
                    self.render_connectors(layer, next_layer, box_width, max_per_row);
                lines.extend(connector_lines);
            }
        }

        // Legend
        lines.push(String::new());
        lines.push(
            "  Legend: [*] merged  [~] in flight  [ ] pending  [?] awaiting merge  [!] failed"
                .to_string(),
        );

        lines
    }

    /// Compute a consistent box width based on the longest content.
    fn compute_box_width(&self) -> usize {
        let min_width = 30;
        let max_width = 44;
        let longest = self
            .nodes
            .values()
            .map(|n| {
                let issue_label = format!("#{}", n.issue_number);
                // State indicator (3) + space + issue label + 2 spaces + title
                3 + 1 + issue_label.len() + 2 + n.title.len()
            })
            .max()
            .unwrap_or(min_width);
        longest.clamp(min_width, max_width)
    }

    /// Render a single node as a Unicode box.
    fn render_box(&self, issue: u32, width: usize) -> Vec<String> {
        let Some(node) = self.nodes.get(&issue) else {
            return vec![];
        };

        let state_char = match node.state {
            NodeState::Merged => '*',
            NodeState::InFlight => '~',
            NodeState::Pending => ' ',
            NodeState::AwaitingMerge => '?',
            NodeState::Failed => '!',
        };

        let blocked = if self.is_blocked(issue) { " BLOCKED" } else { "" };

        let issue_label = format!("#{issue}");
        let title_line = format!("[{state_char}] {issue_label}  {}", node.title);
        let title_truncated = if title_line.len() > width {
            format!("{}..", &title_line[..width - 2])
        } else {
            title_line
        };

        // Second line: area + PR + blocked
        let mut detail_parts = vec![node.area.clone()];
        if let Some(pr) = node.pr_number {
            detail_parts.push(format!("PR #{pr}"));
        }
        let mut detail_line = detail_parts.join("  ");
        if !blocked.is_empty() {
            detail_line.push_str(blocked);
        }
        let detail_truncated = if detail_line.len() > width {
            format!("{}..", &detail_line[..width - 2])
        } else {
            detail_line
        };

        let top = format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(width));
        let mid = format!("\u{2502}{title_truncated:<width$}\u{2502}");
        let mid2 = format!("\u{2502}{detail_truncated:<width$}\u{2502}");
        let bot = format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(width));

        vec![top, mid, mid2, bot]
    }

    /// Render connector lines between two adjacent layers.
    fn render_connectors(
        &self,
        from_layer: &[u32],
        to_layer: &[u32],
        box_width: usize,
        max_per_row: usize,
    ) -> Vec<String> {
        // For each node in to_layer, find which nodes in from_layer it depends on.
        // We draw simple vertical/horizontal pipe connectors.
        let full_box_width = box_width + 2; // include border chars
        let spacing = 2usize;

        // Compute center x position for each node, wrapping by max_per_row
        let from_centers: Vec<(u32, usize)> = from_layer
            .iter()
            .enumerate()
            .map(|(i, &num)| {
                let col = i % max_per_row;
                let x = col * (full_box_width + spacing) + full_box_width / 2;
                (num, x)
            })
            .collect();
        let to_centers: Vec<(u32, usize)> = to_layer
            .iter()
            .enumerate()
            .map(|(i, &num)| {
                let col = i % max_per_row;
                let x = col * (full_box_width + spacing) + full_box_width / 2;
                (num, x)
            })
            .collect();

        // Collect all edges between these two layers (dedup for wrapped columns)
        let mut connections: Vec<(usize, usize)> = Vec::new();
        for &(to_num, to_x) in &to_centers {
            let deps = self.dependencies(to_num);
            for &(from_num, from_x) in &from_centers {
                if deps.contains(&from_num) {
                    connections.push((from_x, to_x));
                }
            }
        }
        connections.sort_unstable();
        connections.dedup();

        if connections.is_empty() {
            return lines_with_gap(1);
        }

        let max_x =
            from_centers.iter().chain(to_centers.iter()).map(|(_, x)| *x).max().unwrap_or(0)
                + full_box_width;
        let width = max_x + 4;

        // Track which columns are sources and targets
        let mut source_cols: HashSet<usize> = HashSet::new();
        let mut target_cols: HashSet<usize> = HashSet::new();
        for &(from_x, to_x) in &connections {
            source_cols.insert(from_x);
            target_cols.insert(to_x);
        }

        // Row 1: vertical pipes from source boxes
        let mut row1 = vec![' '; width];
        for &col in &source_cols {
            row1[col + 2] = '\u{2502}'; // │
        }

        // Row 2: build direction flags per column, then convert to box-drawing
        let row2 = build_connector_row(&connections, &source_cols, &target_cols, width);

        // Row 3: arrows into target boxes
        let mut row3 = vec![' '; width];
        for &col in &target_cols {
            row3[col + 2] = '\u{25bc}'; // ▼
        }

        vec![
            format!("  {}", row1.iter().collect::<String>().trim_end()),
            format!("  {}", row2.trim_end()),
            format!("  {}", row3.iter().collect::<String>().trim_end()),
            String::new(),
        ]
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

/// Build the horizontal connector row between two layers.
///
/// For each column, compute which directions (up/down/left/right) have connections,
/// then pick the appropriate Unicode box-drawing character.
fn build_connector_row(
    connections: &[(usize, usize)],
    source_cols: &HashSet<usize>,
    target_cols: &HashSet<usize>,
    width: usize,
) -> String {
    let mut dirs = vec![0u8; width];

    for &col in source_cols {
        dirs[col + 2] |= DIR_UP;
    }
    for &col in target_cols {
        dirs[col + 2] |= DIR_DOWN;
    }

    for &(from_x, to_x) in connections {
        if from_x == to_x {
            continue;
        }
        let (lo, hi) = if from_x < to_x { (from_x, to_x) } else { (to_x, from_x) };
        dirs[lo + 2] |= DIR_RIGHT;
        dirs[hi + 2] |= DIR_LEFT;
        for col in (lo + 1)..hi {
            dirs[col + 2] |= DIR_LEFT | DIR_RIGHT;
        }
    }

    dirs.iter().map(|&d| box_drawing_char(d)).collect()
}

const DIR_UP: u8 = 0b1000;
const DIR_DOWN: u8 = 0b0100;
const DIR_LEFT: u8 = 0b0010;
const DIR_RIGHT: u8 = 0b0001;

/// Map a direction bitmask to the appropriate Unicode box-drawing character.
const fn box_drawing_char(dirs: u8) -> char {
    match dirs {
        0b1100 => '\u{2502}', // │  up+down
        0b0011 => '\u{2500}', // ─  left+right
        0b1001 => '\u{2514}', // └  up+right
        0b1010 => '\u{2518}', // ┘  up+left
        0b0101 => '\u{250c}', // ┌  down+right
        0b0110 => '\u{2510}', // ┐  down+left
        0b1110 => '\u{2524}', // ┤  up+down+left
        0b1101 => '\u{251c}', // ├  up+down+right
        0b0111 => '\u{252c}', // ┬  down+left+right
        0b1011 => '\u{2534}', // ┴  up+left+right
        0b1111 => '\u{253c}', // ┼  all
        0b1000 => '\u{2575}', // ╵  up only
        0b0100 => '\u{2577}', // ╷  down only
        0b0010 => '\u{2574}', // ╴  left only
        0b0001 => '\u{2576}', // ╶  right only
        _ => ' ',
    }
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(120)
}

fn lines_with_gap(count: usize) -> Vec<String> {
    vec![String::new(); count]
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

    fn make_named_node(num: u32, title: &str, area: &str) -> GraphNode {
        GraphNode {
            issue_number: num,
            title: title.to_string(),
            area: area.to_string(),
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

    // --- display_layered tests ---

    #[test]
    fn display_layered_empty_graph() {
        let g = DependencyGraph::new("test");
        let lines = g.display_layered();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("empty graph"));
    }

    #[test]
    fn display_layered_single_node() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Add auth", "backend"));
        let lines = g.display_layered();
        let text = lines.join("\n");
        assert!(text.contains("1 issues"));
        assert!(text.contains("Layer 0"));
        assert!(text.contains("#1"));
        assert!(text.contains("Add auth"));
        assert!(text.contains("Legend"));
    }

    #[test]
    fn display_layered_linear_chain() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Database schema", "db"));
        g.add_node(make_named_node(2, "API endpoints", "backend"));
        g.add_node(make_named_node(3, "Frontend views", "frontend"));
        g.add_edge(2, 1);
        g.add_edge(3, 2);

        let lines = g.display_layered();
        let text = lines.join("\n");
        // Should have 3 layers
        assert!(text.contains("Layer 0"));
        assert!(text.contains("Layer 1"));
        assert!(text.contains("Layer 2"));
        // All issues present
        assert!(text.contains("#1"));
        assert!(text.contains("#2"));
        assert!(text.contains("#3"));
    }

    #[test]
    fn display_layered_diamond_dag() {
        // 1 is root, 2 and 3 depend on 1, 4 depends on 2 and 3
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Core lib", "core"));
        g.add_node(make_named_node(2, "Auth module", "auth"));
        g.add_node(make_named_node(3, "Logging module", "infra"));
        g.add_node(make_named_node(4, "Integration", "all"));
        g.add_edge(2, 1);
        g.add_edge(3, 1);
        g.add_edge(4, 2);
        g.add_edge(4, 3);

        let lines = g.display_layered();
        let text = lines.join("\n");
        assert!(text.contains("Layer 0"));
        assert!(text.contains("Layer 1"));
        assert!(text.contains("Layer 2"));
        assert!(text.contains("4 issues"));
        // Layer 1 should have both #2 and #3 side by side
        assert!(text.contains("#2"));
        assert!(text.contains("#3"));
    }

    #[test]
    fn display_layered_independent_nodes() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Fix typo", "docs"));
        g.add_node(make_named_node(2, "Add lint", "ci"));
        g.add_node(make_named_node(3, "Bump deps", "deps"));

        let lines = g.display_layered();
        let text = lines.join("\n");
        // All in layer 0, no other layers
        assert!(text.contains("Layer 0 (no deps)"));
        assert!(!text.contains("Layer 1"));
        // All three boxes rendered
        assert!(text.contains("#1"));
        assert!(text.contains("#2"));
        assert!(text.contains("#3"));
    }

    #[test]
    fn display_layered_shows_state_indicators() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Done thing", "core"));
        g.add_node(make_named_node(2, "Running thing", "core"));
        g.add_node(make_named_node(3, "Broken thing", "core"));
        g.add_node(make_named_node(4, "Waiting thing", "core"));
        g.add_node(make_named_node(5, "Pending thing", "core"));
        g.transition(1, NodeState::Merged);
        g.transition(2, NodeState::InFlight);
        g.transition(3, NodeState::Failed);
        g.transition(4, NodeState::AwaitingMerge);

        let lines = g.display_layered();
        let text = lines.join("\n");
        assert!(text.contains("[*]")); // merged
        assert!(text.contains("[~]")); // in_flight
        assert!(text.contains("[!]")); // failed
        assert!(text.contains("[?]")); // awaiting_merge
        assert!(text.contains("[ ]")); // pending
    }

    #[test]
    fn display_layered_shows_pr_number() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Has PR", "core"));
        g.set_pr_number(1, 42);

        let lines = g.display_layered();
        let text = lines.join("\n");
        assert!(text.contains("PR #42"));
    }

    #[test]
    fn display_layered_summary_counts() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "A", "core"));
        g.add_node(make_named_node(2, "B", "core"));
        g.add_node(make_named_node(3, "C", "core"));
        g.transition(1, NodeState::Merged);
        g.transition(2, NodeState::InFlight);
        g.transition(3, NodeState::Failed);

        let lines = g.display_layered();
        let text = lines.join("\n");
        assert!(text.contains("3 issues"));
        assert!(text.contains("1 merged"));
        assert!(text.contains("1 in flight"));
        assert!(text.contains("1 failed"));
    }

    #[test]
    fn display_layered_shows_blocked() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Root", "core"));
        g.add_node(make_named_node(2, "Blocked child", "core"));
        g.add_edge(2, 1);
        g.transition(1, NodeState::Failed);

        let lines = g.display_layered();
        let text = lines.join("\n");
        assert!(text.contains("BLOCKED"));
    }

    #[test]
    fn display_layered_has_connectors_between_layers() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Root", "core"));
        g.add_node(make_named_node(2, "Child", "core"));
        g.add_edge(2, 1);

        let lines = g.display_layered();
        let text = lines.join("\n");
        // Should contain connector characters between layers
        assert!(text.contains('\u{25bc}')); // ▼ arrow
    }

    #[test]
    fn display_layered_box_drawing_chars() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_named_node(1, "Test node", "core"));

        let lines = g.display_layered();
        let text = lines.join("\n");
        // Box corners
        assert!(text.contains('\u{250c}')); // ┌
        assert!(text.contains('\u{2510}')); // ┐
        assert!(text.contains('\u{2514}')); // └
        assert!(text.contains('\u{2518}')); // ┘
        assert!(text.contains('\u{2500}')); // ─
        assert!(text.contains('\u{2502}')); // │
    }

    #[test]
    fn compute_layers_linear() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_edge(2, 1);
        g.add_edge(3, 2);

        let layers = g.compute_layers();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![1]);
        assert_eq!(layers[1], vec![2]);
        assert_eq!(layers[2], vec![3]);
    }

    #[test]
    fn compute_layers_diamond() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));
        g.add_node(make_node(4));
        g.add_edge(2, 1);
        g.add_edge(3, 1);
        g.add_edge(4, 2);
        g.add_edge(4, 3);

        let layers = g.compute_layers();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![1]);
        assert_eq!(layers[1], vec![2, 3]);
        assert_eq!(layers[2], vec![4]);
    }

    #[test]
    fn compute_layers_independent() {
        let mut g = DependencyGraph::new("test");
        g.add_node(make_node(1));
        g.add_node(make_node(2));
        g.add_node(make_node(3));

        let layers = g.compute_layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0], vec![1, 2, 3]);
    }

    #[test]
    fn display_layered_wraps_wide_layers() {
        // Build a graph with many independent nodes so wrapping kicks in
        // regardless of terminal width.
        let mut g = DependencyGraph::new("test");
        for i in 1..=12 {
            g.add_node(make_named_node(i, &format!("Task {i}"), "area"));
        }

        let lines = g.display_layered();
        let text = lines.join("\n");

        // All nodes should appear
        for i in 1..=12 {
            assert!(text.contains(&format!("#{i}")), "missing #{i}");
        }

        // No single line should exceed the detected terminal width.
        // Use chars().count() since box-drawing chars are multi-byte UTF-8.
        let term_w = super::terminal_width();
        let max_line = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        assert!(max_line <= term_w, "line too wide: {max_line} chars (expected <= {term_w})");
    }

    #[test]
    fn display_layered_wrapping_preserves_connectors() {
        // Layer 0: 6 independent nodes (will wrap at default 120 cols).
        // Layer 1: 1 node that depends on node 1.
        let mut g = DependencyGraph::new("test");
        for i in 1..=6 {
            g.add_node(make_named_node(i, &format!("Task {i}"), "area"));
        }
        g.add_node(make_named_node(7, "Dependent", "area"));
        g.add_edge(7, 1);

        let lines = g.display_layered();
        let text = lines.join("\n");

        // Connector arrow should still be present
        assert!(text.contains('\u{25bc}'), "missing connector arrow");
        // Both layers should appear
        assert!(text.contains("Layer 0"));
        assert!(text.contains("Layer 1"));
    }
}
