use askama::Template;

use super::AgentContext;

#[derive(Template)]
#[template(path = "reviewer.txt")]
struct ReviewerPrompt<'a> {
    ctx: &'a AgentContext,
}

pub fn build_prompt(ctx: &AgentContext) -> String {
    let tmpl = ReviewerPrompt { ctx };
    tmpl.render().expect("reviewer template render failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentContext;

    fn sample_context() -> AgentContext {
        AgentContext {
            issue_number: 42,
            issue_title: "Fix bug".to_string(),
            issue_body: "details".to_string(),
            branch: "oven/issue-42-abc".to_string(),
            pr_number: None,
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
        }
    }

    #[test]
    fn prompt_includes_review_instructions() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("reviewer agent"));
        assert!(prompt.contains("#42"));
        assert!(prompt.contains("<issue_title>Fix bug</issue_title>"));
        assert!(prompt.contains("findings"));
    }

    #[test]
    fn prompt_includes_all_checklist_categories() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("Pattern Consistency"));
        assert!(prompt.contains("Error Handling"));
        assert!(prompt.contains("Test Coverage"));
        assert!(prompt.contains("Code Quality"));
        assert!(prompt.contains("Security"));
        assert!(prompt.contains("Acceptance Criteria"));
    }

    #[test]
    fn prompt_includes_severity_guide() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("**critical**: Must fix before merge"));
        assert!(prompt.contains("**warning**: Should fix"));
        assert!(prompt.contains("**info**: Noteworthy"));
    }

    #[test]
    fn prompt_includes_json_output_format() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("\"severity\": \"critical\""));
        assert!(prompt.contains("\"file_path\""));
        assert!(prompt.contains("\"line_number\""));
    }

    #[test]
    fn prompt_includes_specificity_requirement() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("Specificity Requirement"));
    }
}
