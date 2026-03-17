use anyhow::{Context, Result};
use rusqlite::{Connection, params};

/// State of a node in the dependency graph (stored as text in `SQLite`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeState {
    Pending,
    InFlight,
    AwaitingMerge,
    Merged,
    Failed,
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::InFlight => "in_flight",
            Self::AwaitingMerge => "awaiting_merge",
            Self::Merged => "merged",
            Self::Failed => "failed",
        })
    }
}

impl std::str::FromStr for NodeState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "in_flight" => Ok(Self::InFlight),
            "awaiting_merge" => Ok(Self::AwaitingMerge),
            "merged" => Ok(Self::Merged),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown node state: {other}"),
        }
    }
}

/// A row from the `graph_nodes` table.
#[derive(Debug, Clone)]
pub struct GraphNodeRow {
    pub issue_number: u32,
    pub session_id: String,
    pub state: NodeState,
    pub pr_number: Option<u32>,
    pub run_id: Option<String>,
    pub title: String,
    pub area: String,
    pub predicted_files: Vec<String>,
    pub has_migration: bool,
    pub complexity: String,
}

pub fn insert_node(conn: &Connection, session_id: &str, node: &GraphNodeRow) -> Result<()> {
    let files_json =
        serde_json::to_string(&node.predicted_files).context("serializing predicted_files")?;
    conn.execute(
        "INSERT OR REPLACE INTO graph_nodes \
         (issue_number, session_id, state, pr_number, run_id, title, area, \
          predicted_files, has_migration, complexity) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            node.issue_number,
            session_id,
            node.state.to_string(),
            node.pr_number,
            node.run_id,
            node.title,
            node.area,
            files_json,
            node.has_migration,
            node.complexity,
        ],
    )
    .context("inserting graph node")?;
    Ok(())
}

pub fn update_node_state(
    conn: &Connection,
    session_id: &str,
    issue_number: u32,
    state: NodeState,
) -> Result<()> {
    conn.execute(
        "UPDATE graph_nodes SET state = ?1 WHERE issue_number = ?2 AND session_id = ?3",
        params![state.to_string(), issue_number, session_id],
    )
    .context("updating graph node state")?;
    Ok(())
}

pub fn update_node_pr(
    conn: &Connection,
    session_id: &str,
    issue_number: u32,
    pr_number: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE graph_nodes SET pr_number = ?1 WHERE issue_number = ?2 AND session_id = ?3",
        params![pr_number, issue_number, session_id],
    )
    .context("updating graph node PR")?;
    Ok(())
}

pub fn update_node_run_id(
    conn: &Connection,
    session_id: &str,
    issue_number: u32,
    run_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE graph_nodes SET run_id = ?1 WHERE issue_number = ?2 AND session_id = ?3",
        params![run_id, issue_number, session_id],
    )
    .context("updating graph node run_id")?;
    Ok(())
}

pub fn insert_edge(
    conn: &Connection,
    session_id: &str,
    from_issue: u32,
    to_issue: u32,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO graph_edges (session_id, from_issue, to_issue) \
         VALUES (?1, ?2, ?3)",
        params![session_id, from_issue, to_issue],
    )
    .context("inserting graph edge")?;
    Ok(())
}

pub fn get_nodes(conn: &Connection, session_id: &str) -> Result<Vec<GraphNodeRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT issue_number, session_id, state, pr_number, run_id, title, area, \
             predicted_files, has_migration, complexity \
             FROM graph_nodes WHERE session_id = ?1 ORDER BY issue_number",
        )
        .context("preparing get_nodes")?;

    let rows = stmt
        .query_map(params![session_id], |row| {
            let state_str: String = row.get(2)?;
            let files_json: String = row.get(7)?;
            Ok(GraphNodeRow {
                issue_number: row.get(0)?,
                session_id: row.get(1)?,
                state: state_str.parse().map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        2,
                        "state".to_string(),
                        rusqlite::types::Type::Text,
                    )
                })?,
                pr_number: row.get(3)?,
                run_id: row.get(4)?,
                title: row.get(5)?,
                area: row.get(6)?,
                predicted_files: serde_json::from_str(&files_json).unwrap_or_default(),
                has_migration: row.get(8)?,
                complexity: row.get(9)?,
            })
        })
        .context("querying graph nodes")?;

    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting graph nodes")
}

pub fn get_edges(conn: &Connection, session_id: &str) -> Result<Vec<(u32, u32)>> {
    let mut stmt = conn
        .prepare(
            "SELECT from_issue, to_issue FROM graph_edges \
             WHERE session_id = ?1 ORDER BY from_issue, to_issue",
        )
        .context("preparing get_edges")?;

    let rows = stmt
        .query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("querying graph edges")?;

    rows.collect::<std::result::Result<Vec<_>, _>>().context("collecting graph edges")
}

pub fn delete_session(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute("DELETE FROM graph_edges WHERE session_id = ?1", params![session_id])
        .context("deleting graph edges")?;
    conn.execute("DELETE FROM graph_nodes WHERE session_id = ?1", params![session_id])
        .context("deleting graph nodes")?;
    Ok(())
}

/// Find the most recent session that has at least one non-terminal node.
pub fn get_active_session(conn: &Connection) -> Result<Option<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT session_id FROM graph_nodes \
             WHERE state NOT IN ('merged', 'failed') \
             LIMIT 1",
        )
        .context("preparing get_active_session")?;

    let mut rows = stmt.query_map([], |row| row.get(0)).context("querying active session")?;
    match rows.next() {
        Some(row) => Ok(Some(row.context("reading session_id")?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn test_db() -> Connection {
        db::open_in_memory().unwrap()
    }

    fn sample_node(issue: u32, session: &str) -> GraphNodeRow {
        GraphNodeRow {
            issue_number: issue,
            session_id: session.to_string(),
            state: NodeState::Pending,
            pr_number: None,
            run_id: None,
            title: format!("Issue #{issue}"),
            area: "test".to_string(),
            predicted_files: vec!["src/main.rs".to_string()],
            has_migration: false,
            complexity: "full".to_string(),
        }
    }

    #[test]
    fn insert_and_get_nodes() {
        let conn = test_db();
        let node = sample_node(1, "sess1");
        insert_node(&conn, "sess1", &node).unwrap();

        let nodes = get_nodes(&conn, "sess1").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].issue_number, 1);
        assert_eq!(nodes[0].state, NodeState::Pending);
        assert_eq!(nodes[0].predicted_files, vec!["src/main.rs"]);
    }

    #[test]
    fn insert_and_get_edges() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        insert_node(&conn, "sess1", &sample_node(2, "sess1")).unwrap();
        insert_edge(&conn, "sess1", 2, 1).unwrap();

        let edges = get_edges(&conn, "sess1").unwrap();
        assert_eq!(edges, vec![(2, 1)]);
    }

    #[test]
    fn update_state_persists() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        update_node_state(&conn, "sess1", 1, NodeState::InFlight).unwrap();

        let nodes = get_nodes(&conn, "sess1").unwrap();
        assert_eq!(nodes[0].state, NodeState::InFlight);
    }

    #[test]
    fn update_pr_persists() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        update_node_pr(&conn, "sess1", 1, 42).unwrap();

        let nodes = get_nodes(&conn, "sess1").unwrap();
        assert_eq!(nodes[0].pr_number, Some(42));
    }

    #[test]
    fn update_run_id_persists() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        update_node_run_id(&conn, "sess1", 1, "abc123").unwrap();

        let nodes = get_nodes(&conn, "sess1").unwrap();
        assert_eq!(nodes[0].run_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn session_isolation() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        insert_node(&conn, "sess2", &sample_node(2, "sess2")).unwrap();

        assert_eq!(get_nodes(&conn, "sess1").unwrap().len(), 1);
        assert_eq!(get_nodes(&conn, "sess2").unwrap().len(), 1);
        assert_eq!(get_nodes(&conn, "sess3").unwrap().len(), 0);
    }

    #[test]
    fn delete_session_removes_all() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        insert_node(&conn, "sess1", &sample_node(2, "sess1")).unwrap();
        insert_edge(&conn, "sess1", 2, 1).unwrap();

        delete_session(&conn, "sess1").unwrap();
        assert!(get_nodes(&conn, "sess1").unwrap().is_empty());
        assert!(get_edges(&conn, "sess1").unwrap().is_empty());
    }

    #[test]
    fn get_active_session_finds_non_terminal() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        assert_eq!(get_active_session(&conn).unwrap().as_deref(), Some("sess1"));
    }

    #[test]
    fn get_active_session_skips_all_terminal() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        update_node_state(&conn, "sess1", 1, NodeState::Merged).unwrap();
        assert!(get_active_session(&conn).unwrap().is_none());
    }

    #[test]
    fn duplicate_edge_is_idempotent() {
        let conn = test_db();
        insert_node(&conn, "sess1", &sample_node(1, "sess1")).unwrap();
        insert_node(&conn, "sess1", &sample_node(2, "sess1")).unwrap();
        insert_edge(&conn, "sess1", 2, 1).unwrap();
        insert_edge(&conn, "sess1", 2, 1).unwrap(); // no error

        let edges = get_edges(&conn, "sess1").unwrap();
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn node_state_display_roundtrip() {
        let states = [
            NodeState::Pending,
            NodeState::InFlight,
            NodeState::AwaitingMerge,
            NodeState::Merged,
            NodeState::Failed,
        ];
        for state in states {
            let s = state.to_string();
            let parsed: NodeState = s.parse().unwrap();
            assert_eq!(state, parsed);
        }
    }

    #[test]
    fn upsert_overwrites_existing_node() {
        let conn = test_db();
        let mut node = sample_node(1, "sess1");
        insert_node(&conn, "sess1", &node).unwrap();

        node.title = "Updated title".to_string();
        node.state = NodeState::InFlight;
        insert_node(&conn, "sess1", &node).unwrap();

        let nodes = get_nodes(&conn, "sess1").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].title, "Updated title");
        assert_eq!(nodes[0].state, NodeState::InFlight);
    }

    #[test]
    fn predicted_files_roundtrip_empty() {
        let conn = test_db();
        let mut node = sample_node(1, "sess1");
        node.predicted_files = vec![];
        insert_node(&conn, "sess1", &node).unwrap();

        let nodes = get_nodes(&conn, "sess1").unwrap();
        assert!(nodes[0].predicted_files.is_empty());
    }
}
