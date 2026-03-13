use askama::Template;

use super::AgentContext;

#[derive(Template)]
#[template(path = "implementer.txt")]
struct ImplementerPrompt<'a> {
    ctx: &'a AgentContext,
}

pub fn build_prompt(ctx: &AgentContext) -> String {
    let tmpl = ImplementerPrompt { ctx };
    tmpl.render().expect("implementer template render failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_context() -> AgentContext {
        AgentContext {
            issue_number: 42,
            issue_title: "Add retry logic".to_string(),
            issue_body: "Implement retry for API calls".to_string(),
            branch: "oven/issue-42-abcd1234".to_string(),
            pr_number: Some(99),
            test_command: Some("cargo test".to_string()),
            lint_command: Some("cargo clippy".to_string()),
            review_findings: None,
            cycle: 1,
        }
    }

    #[test]
    fn prompt_includes_issue_details() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("<issue_number>42</issue_number>"));
        assert!(prompt.contains("<issue_title>Add retry logic</issue_title>"));
        assert!(prompt.contains("Implement retry for API calls"));
        assert!(prompt.contains("oven/issue-42-abcd1234"));
        assert!(prompt.contains("PR: #99"));
    }

    #[test]
    fn prompt_includes_test_and_lint_commands() {
        let prompt = build_prompt(&sample_context());
        assert!(prompt.contains("cargo test"));
        assert!(prompt.contains("cargo clippy"));
    }

    #[test]
    fn prompt_without_test_command() {
        let mut ctx = sample_context();
        ctx.test_command = None;
        let prompt = build_prompt(&ctx);
        assert!(!prompt.contains("cargo test"));
    }
}
