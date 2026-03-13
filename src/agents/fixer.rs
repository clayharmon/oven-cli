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

    #[test]
    fn prompt_includes_findings() {
        let ctx = AgentContext {
            issue_number: 42,
            issue_title: "Fix bug".to_string(),
            issue_body: "details".to_string(),
            branch: "oven/issue-42-abc".to_string(),
            pr_number: None,
            test_command: Some("cargo test".to_string()),
            lint_command: None,
            review_findings: None,
            cycle: 1,
        };
        let findings = vec![ReviewFinding {
            id: 1,
            agent_run_id: 1,
            severity: "critical".to_string(),
            category: "bug".to_string(),
            file_path: Some("src/main.rs".to_string()),
            line_number: Some(10),
            message: "null pointer".to_string(),
            resolved: false,
        }];
        let prompt = build_prompt(&ctx, &findings);
        assert!(prompt.contains("fixer agent"));
        assert!(prompt.contains("[critical] bug"));
        assert!(prompt.contains("src/main.rs:10"));
        assert!(prompt.contains("null pointer"));
        assert!(prompt.contains("cargo test"));
        assert!(prompt.contains("<review_findings>"));
    }
}
