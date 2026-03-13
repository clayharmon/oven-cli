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

    #[test]
    fn prompt_includes_review_instructions() {
        let ctx = AgentContext {
            issue_number: 42,
            issue_title: "Fix bug".to_string(),
            issue_body: "details".to_string(),
            branch: "oven/issue-42-abc".to_string(),
            pr_number: None,
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
        };
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("reviewer agent"));
        assert!(prompt.contains("#42"));
        assert!(prompt.contains("<issue_title>Fix bug</issue_title>"));
        assert!(prompt.contains("critical|warning|info"));
        assert!(prompt.contains("findings"));
    }
}
