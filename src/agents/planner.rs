use anyhow::{Context, Result};
use askama::Template;

use crate::{agents::InFlightIssue, issues::PipelineIssue};

#[derive(Template)]
#[template(path = "planner.txt")]
struct PlannerPrompt<'a> {
    issues: &'a [PipelineIssue],
    in_flight: &'a [InFlightIssue],
}

pub fn build_prompt(issues: &[PipelineIssue], in_flight: &[InFlightIssue]) -> Result<String> {
    let tmpl = PlannerPrompt { issues, in_flight };
    tmpl.render().context("rendering planner template")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agents::Complexity, issues::IssueOrigin};

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
    fn prompt_includes_conflict_detection() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(prompt.contains("Conflict Detection"));
        assert!(prompt.contains("CANNOT parallelize"));
        assert!(prompt.contains("CAN parallelize"));
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
    }

    #[test]
    fn prompt_omits_in_flight_when_empty() {
        let prompt = build_prompt(&sample_issues(), &[]).unwrap();
        assert!(!prompt.contains("<in_flight>"));
    }

    #[test]
    fn prompt_includes_in_flight_context() {
        let in_flight = vec![
            InFlightIssue {
                number: 10,
                title: "Refactor auth".to_string(),
                area: "auth".to_string(),
                predicted_files: vec!["src/auth.rs".to_string(), "src/middleware.rs".to_string()],
                has_migration: false,
                complexity: Complexity::Full,
            },
            InFlightIssue {
                number: 11,
                title: "Add migration".to_string(),
                area: "db".to_string(),
                predicted_files: vec!["src/db/mod.rs".to_string()],
                has_migration: true,
                complexity: Complexity::Simple,
            },
        ];

        let prompt = build_prompt(&sample_issues(), &in_flight).unwrap();
        assert!(prompt.contains("<in_flight>"));
        assert!(prompt.contains("#10: Refactor auth"));
        assert!(prompt.contains("area: auth"));
        assert!(prompt.contains("src/auth.rs"));
        assert!(prompt.contains("src/middleware.rs"));
        assert!(prompt.contains("has_migration: false"));
        assert!(prompt.contains("#11: Add migration"));
        assert!(prompt.contains("has_migration: true"));
        assert!(prompt.contains("in-flight work"));
    }
}
