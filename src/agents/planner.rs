use askama::Template;

use crate::issues::PipelineIssue;

#[derive(Template)]
#[template(path = "planner.txt")]
struct PlannerPrompt<'a> {
    issues: &'a [PipelineIssue],
}

pub fn build_prompt(issues: &[PipelineIssue]) -> String {
    let tmpl = PlannerPrompt { issues };
    tmpl.render().expect("planner template render failed")
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
            },
            PipelineIssue {
                number: 2,
                title: "Fix bug".to_string(),
                body: "crash on startup".to_string(),
                source: IssueOrigin::Github,
                target_repo: None,
            },
        ]
    }

    #[test]
    fn prompt_includes_issue_details() {
        let prompt = build_prompt(&sample_issues());
        assert!(prompt.contains("#1: Add login"));
        assert!(prompt.contains("#2: Fix bug"));
        assert!(prompt.contains("<issue_body>implement login flow</issue_body>"));
        assert!(prompt.contains("<issue_body>crash on startup</issue_body>"));
    }

    #[test]
    fn prompt_includes_complexity_classification() {
        let prompt = build_prompt(&sample_issues());
        assert!(prompt.contains("**simple**"));
        assert!(prompt.contains("**full**"));
        assert!(prompt.contains("Complexity Classification"));
    }

    #[test]
    fn prompt_includes_conflict_detection() {
        let prompt = build_prompt(&sample_issues());
        assert!(prompt.contains("Conflict Detection"));
        assert!(prompt.contains("CANNOT parallelize"));
        assert!(prompt.contains("CAN parallelize"));
    }

    #[test]
    fn prompt_structured_json_output_is_valid() {
        let prompt = build_prompt(&sample_issues());
        // The template includes an example JSON block -- verify it has the new fields
        assert!(prompt.contains("\"complexity\": \"simple\""));
        assert!(prompt.contains("\"has_migration\""));
        assert!(prompt.contains("\"predicted_files\""));
        assert!(prompt.contains("\"area\""));
        assert!(prompt.contains("\"total_issues\""));
        assert!(prompt.contains("\"parallel_capacity\""));
    }
}
