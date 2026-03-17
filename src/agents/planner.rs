use anyhow::{Context, Result};
use askama::Template;

use crate::{agents::GraphContextNode, issues::PipelineIssue};

#[derive(Template)]
#[template(path = "planner.txt")]
struct PlannerPrompt<'a> {
    issues: &'a [PipelineIssue],
    graph_context: &'a [GraphContextNode],
}

pub fn build_prompt(
    issues: &[PipelineIssue],
    graph_context: &[GraphContextNode],
) -> Result<String> {
    let tmpl = PlannerPrompt { issues, graph_context };
    tmpl.render().context("rendering planner template")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::IssueOrigin;

    fn sample_issues() -> Vec<PipelineIssue> {
        vec![
            PipelineIssue {
                number: 1,
                title: "Add login".to_string(),
                body: "implement login flow".to_string(),
                source: IssueOrigin::Github,
                target_repo: None,
                author: None,
            },
            PipelineIssue {
                number: 2,
                title: "Fix bug".to_string(),
                body: "crash on startup".to_string(),
                source: IssueOrigin::Github,
                target_repo: None,
                author: None,
            },
        ]
    }

    #[test]
    fn prompt_includes_issue_details() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(prompt.contains("#1: Add login"));
        assert!(prompt.contains("#2: Fix bug"));
        assert!(prompt.contains("<issue_body>implement login flow</issue_body>"));
        assert!(prompt.contains("<issue_body>crash on startup</issue_body>"));
    }

    #[test]
    fn prompt_includes_complexity_classification() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(prompt.contains("**simple**"));
        assert!(prompt.contains("**full**"));
        assert!(prompt.contains("Complexity Classification"));
    }

    #[test]
    fn prompt_includes_dependency_analysis() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(prompt.contains("Dependency Analysis"));
        assert!(prompt.contains("depends_on"));
    }

    #[test]
    fn prompt_structured_json_output_is_valid() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(prompt.contains("\"complexity\": \"simple\""));
        assert!(prompt.contains("\"has_migration\""));
        assert!(prompt.contains("\"predicted_files\""));
        assert!(prompt.contains("\"area\""));
        assert!(prompt.contains("\"total_issues\""));
        assert!(prompt.contains("\"parallel_capacity\""));
        assert!(prompt.contains("\"depends_on\""));
        assert!(prompt.contains("\"nodes\""));
    }

    #[test]
    fn prompt_omits_graph_context_when_empty() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(!prompt.contains("<graph_state>"));
    }

    #[test]
    fn prompt_includes_graph_context() {
        let graph_ctx = vec![
            GraphContextNode {
                number: 10,
                title: "Refactor auth".to_string(),
                state: crate::db::graph::NodeState::InFlight,
                area: "auth".to_string(),
                predicted_files: vec!["src/auth.rs".to_string(), "src/middleware.rs".to_string()],
                has_migration: false,
                depends_on: vec![],
                target_repo: None,
            },
            GraphContextNode {
                number: 11,
                title: "Add migration".to_string(),
                state: crate::db::graph::NodeState::AwaitingMerge,
                area: "db".to_string(),
                predicted_files: vec!["src/db/mod.rs".to_string()],
                has_migration: true,
                depends_on: vec![10],
                target_repo: Some("backend".to_string()),
            },
        ];

        let prompt = build_prompt(&sample_issues(), &graph_ctx).unwrap();
        assert!(prompt.contains("<graph_state>"));
        assert!(prompt.contains("#10: Refactor auth"));
        assert!(prompt.contains("state: in_flight"));
        assert!(prompt.contains("area: auth"));
        assert!(prompt.contains("src/auth.rs"));
        assert!(prompt.contains("src/middleware.rs"));
        assert!(prompt.contains("has_migration: false"));
        assert!(prompt.contains("#11: Add migration"));
        assert!(prompt.contains("state: awaiting_merge"));
        assert!(prompt.contains("has_migration: true"));
        assert!(prompt.contains("#10"));
        // target_repo only shown when set
        assert!(prompt.contains("target_repo: backend"));
        // node 10 has no target_repo, so it should not appear for that node
        let auth_section =
            prompt.split("#10: Refactor auth").nth(1).unwrap().split('#').next().unwrap();
        assert!(!auth_section.contains("target_repo"));
    }
}
