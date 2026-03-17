use anyhow::{Context, Result};
use askama::Template;

use super::AgentContext;
use crate::db::ReviewFinding;

#[derive(Template)]
#[template(path = "reviewer.txt")]
struct ReviewerPrompt<'a> {
    ctx: &'a AgentContext,
    prior_disputes: &'a [ReviewFinding],
}

pub fn build_prompt(ctx: &AgentContext, prior_disputes: &[ReviewFinding]) -> Result<String> {
    let tmpl = ReviewerPrompt { ctx, prior_disputes };
    tmpl.render().context("rendering reviewer template")
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
            target_repo: None,
            issue_source: "github".to_string(),
            base_branch: "main".to_string(),
        }
    }

    #[test]
    fn prompt_includes_review_instructions() {
        let prompt = build_prompt(&sample_context(), &[]).unwrap();
        assert!(prompt.contains("reviewer agent"));
        assert!(prompt.contains("#42"));
        assert!(prompt.contains("<issue_title>Fix bug</issue_title>"));
        assert!(prompt.contains("findings"));
    }

    #[test]
    fn prompt_includes_all_checklist_categories() {
        let prompt = build_prompt(&sample_context(), &[]).unwrap();
        assert!(prompt.contains("Pattern Consistency"));
        assert!(prompt.contains("Error Handling"));
        assert!(prompt.contains("Test Coverage"));
        assert!(prompt.contains("Code Quality"));
        assert!(prompt.contains("Security"));
        assert!(prompt.contains("Acceptance Criteria"));
    }

    #[test]
    fn prompt_includes_severity_guide() {
        let prompt = build_prompt(&sample_context(), &[]).unwrap();
        assert!(prompt.contains("**critical**: Must fix before merge"));
        assert!(prompt.contains("**warning**: Should fix"));
        assert!(prompt.contains("**info**: Noteworthy"));
    }

    #[test]
    fn prompt_includes_json_output_format() {
        let prompt = build_prompt(&sample_context(), &[]).unwrap();
        assert!(prompt.contains("\"severity\": \"critical\""));
        assert!(prompt.contains("\"file_path\""));
        assert!(prompt.contains("\"line_number\""));
    }

    #[test]
    fn prompt_includes_specificity_requirement() {
        let prompt = build_prompt(&sample_context(), &[]).unwrap();
        assert!(prompt.contains("Specificity Requirement"));
    }

    #[test]
    fn prompt_includes_prior_disputes_when_present() {
        let disputes = vec![ReviewFinding {
            id: 1,
            agent_run_id: 1,
            severity: "critical".to_string(),
            category: "convention".to_string(),
            file_path: Some("src/main.rs".to_string()),
            line_number: Some(42),
            message: "Missing estimatedItemSize".to_string(),
            resolved: true,
            dispute_reason: Some("FlashList v2 removed this prop".to_string()),
        }];
        let prompt = build_prompt(&sample_context(), &disputes).unwrap();
        assert!(prompt.contains("Prior Disputes"));
        assert!(prompt.contains("FlashList v2 removed this prop"));
        assert!(prompt.contains("Missing estimatedItemSize"));
        assert!(prompt.contains("compiler is authoritative"));
    }

    #[test]
    fn prompt_omits_disputes_section_when_empty() {
        let prompt = build_prompt(&sample_context(), &[]).unwrap();
        assert!(!prompt.contains("Prior Disputes"));
    }
}
