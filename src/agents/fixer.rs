use askama::Template;

use super::AgentContext;
use crate::db::ReviewFinding;

#[derive(Template)]
#[template(path = "fixer.txt")]
struct FixerPrompt<'a> {
    ctx: &'a AgentContext,
    findings: &'a [ReviewFinding],
}

pub fn build_prompt(ctx: &AgentContext, findings: &[ReviewFinding]) -> String {
    let tmpl = FixerPrompt { ctx, findings };
    tmpl.render().expect("fixer template render failed")
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
            test_command: Some("cargo test".to_string()),
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
        }
    }

    fn sample_findings() -> Vec<ReviewFinding> {
        vec![ReviewFinding {
            id: 1,
            agent_run_id: 1,
            severity: "critical".to_string(),
            category: "bug".to_string(),
            file_path: Some("src/main.rs".to_string()),
            line_number: Some(10),
            message: "null pointer".to_string(),
            resolved: false,
        }]
    }

    #[test]
    fn prompt_includes_findings() {
        let prompt = build_prompt(&sample_context(), &sample_findings());
        assert!(prompt.contains("fixer agent"));
        assert!(prompt.contains("[critical] bug"));
        assert!(prompt.contains("src/main.rs:10"));
        assert!(prompt.contains("null pointer"));
        assert!(prompt.contains("cargo test"));
        assert!(prompt.contains("<review_findings>"));
    }

    #[test]
    fn prompt_includes_scope_discipline() {
        let prompt = build_prompt(&sample_context(), &sample_findings());
        assert!(prompt.contains("Scope Discipline"));
        assert!(prompt.contains("Do NOT refactor code that wasn't flagged"));
    }

    #[test]
    fn prompt_includes_verification_section() {
        let prompt = build_prompt(&sample_context(), &sample_findings());
        assert!(prompt.contains("Verification"));
        assert!(prompt.contains("git diff main --stat"));
    }

    #[test]
    fn prompt_includes_skip_guidance() {
        let prompt = build_prompt(&sample_context(), &sample_findings());
        assert!(prompt.contains("Handling Unclear Findings"));
        assert!(prompt.contains("Skip it"));
    }
}
