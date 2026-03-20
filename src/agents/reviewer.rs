use anyhow::{Context, Result};
use askama::Template;

use super::AgentContext;
use crate::db::ReviewFinding;

#[derive(Template)]
#[template(path = "reviewer.txt")]
struct ReviewerPrompt<'a> {
    ctx: &'a AgentContext,
    prior_addressed: &'a [ReviewFinding],
    prior_disputes: &'a [ReviewFinding],
    prior_unresolved: &'a [ReviewFinding],
    pre_fix_ref: Option<&'a str>,
}

pub fn build_prompt(
    ctx: &AgentContext,
    prior_addressed: &[ReviewFinding],
    prior_disputes: &[ReviewFinding],
    prior_unresolved: &[ReviewFinding],
    pre_fix_ref: Option<&str>,
) -> Result<String> {
    let tmpl =
        ReviewerPrompt { ctx, prior_addressed, prior_disputes, prior_unresolved, pre_fix_ref };
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
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
        assert!(prompt.contains("reviewer agent"));
        assert!(prompt.contains("#42"));
        assert!(prompt.contains("<issue_title>Fix bug</issue_title>"));
        assert!(prompt.contains("findings"));
    }

    #[test]
    fn prompt_includes_all_checklist_categories() {
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
        assert!(prompt.contains("Pattern Consistency"));
        assert!(prompt.contains("Error Handling"));
        assert!(prompt.contains("Test Coverage"));
        assert!(prompt.contains("Code Quality"));
        assert!(prompt.contains("Security"));
        assert!(prompt.contains("Acceptance Criteria"));
    }

    #[test]
    fn prompt_includes_severity_guide() {
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
        assert!(prompt.contains("**critical**: Must fix before merge"));
        assert!(prompt.contains("**warning**: Should fix"));
        assert!(prompt.contains("**info**: Noteworthy"));
    }

    #[test]
    fn prompt_includes_json_output_format() {
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
        assert!(prompt.contains("\"severity\": \"critical\""));
        assert!(prompt.contains("\"file_path\""));
        assert!(prompt.contains("\"line_number\""));
    }

    #[test]
    fn prompt_includes_specificity_requirement() {
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
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
        let prompt = build_prompt(&sample_context(), &[], &disputes, &[], None).unwrap();
        assert!(prompt.contains("Prior Disputes"));
        assert!(prompt.contains("FlashList v2 removed this prop"));
        assert!(prompt.contains("Missing estimatedItemSize"));
        assert!(prompt.contains("compiler is authoritative"));
    }

    #[test]
    fn prompt_omits_disputes_section_when_empty() {
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
        assert!(!prompt.contains("Prior Disputes"));
    }

    #[test]
    fn prompt_includes_prior_addressed_when_present() {
        let addressed = vec![ReviewFinding {
            id: 1,
            agent_run_id: 1,
            severity: "warning".to_string(),
            category: "bug".to_string(),
            file_path: Some("src/app.rs".to_string()),
            line_number: Some(79),
            message: "has_more hardcoded to true".to_string(),
            resolved: true,
            dispute_reason: Some("Fixed by changing to entries.present?".to_string()),
        }];
        let prompt = build_prompt(&sample_context(), &addressed, &[], &[], None).unwrap();
        assert!(prompt.contains("Prior Addressed Findings"));
        assert!(prompt.contains("Fixed by changing to entries.present?"));
        assert!(prompt.contains("has_more hardcoded to true"));
        assert!(prompt.contains("Anti-Goalpost Rules"));
    }

    #[test]
    fn prompt_omits_addressed_section_when_empty() {
        let prompt = build_prompt(&sample_context(), &[], &[], &[], None).unwrap();
        assert!(!prompt.contains("Prior Addressed Findings"));
        assert!(!prompt.contains("Anti-Goalpost Rules"));
    }

    #[test]
    fn prompt_shows_anti_goalpost_rules_with_disputes_only() {
        let disputes = vec![ReviewFinding {
            id: 1,
            agent_run_id: 1,
            severity: "critical".to_string(),
            category: "convention".to_string(),
            file_path: None,
            line_number: None,
            message: "test".to_string(),
            resolved: true,
            dispute_reason: Some("reason".to_string()),
        }];
        let prompt = build_prompt(&sample_context(), &[], &disputes, &[], None).unwrap();
        assert!(prompt.contains("Anti-Goalpost Rules"));
    }
}
