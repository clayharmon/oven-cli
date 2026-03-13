use askama::Template;

use super::AgentContext;

#[derive(Template)]
#[template(path = "merger.txt")]
struct MergerPrompt<'a> {
    ctx: &'a AgentContext,
    auto_merge: bool,
}

pub fn build_prompt(ctx: &AgentContext, auto_merge: bool) -> String {
    let tmpl = MergerPrompt { ctx, auto_merge };
    tmpl.render().expect("merger template render failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_context() -> AgentContext {
        AgentContext {
            issue_number: 42,
            issue_title: "Fix auth bug".to_string(),
            issue_body: "details".to_string(),
            branch: "oven/issue-42-abc".to_string(),
            pr_number: Some(99),
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
            issue_source: "github".to_string(),
        }
    }

    #[test]
    fn prompt_references_pr_number() {
        let prompt = build_prompt(&sample_context(), false);
        assert!(prompt.contains("PR #99"));
        assert!(prompt.contains("gh pr ready 99"));
        assert!(prompt.contains("#42"));
    }

    #[test]
    fn prompt_without_merge() {
        let prompt = build_prompt(&sample_context(), false);
        assert!(prompt.contains("gh pr ready 99"));
        assert!(!prompt.contains("gh pr merge"));
    }

    #[test]
    fn prompt_with_merge() {
        let prompt = build_prompt(&sample_context(), true);
        assert!(prompt.contains("gh pr ready 99"));
        assert!(prompt.contains("gh pr merge 99"));
        assert!(prompt.contains("--squash"));
        assert!(prompt.contains("--delete-branch"));
    }

    #[test]
    fn prompt_includes_issue_close_when_auto_merge() {
        let prompt = build_prompt(&sample_context(), true);
        assert!(prompt.contains("gh issue close 42"));
    }

    #[test]
    fn prompt_includes_pr_description_update() {
        let prompt = build_prompt(&sample_context(), false);
        assert!(prompt.contains("gh pr edit 99"));
        assert!(prompt.contains("Resolves #42"));
    }

    #[test]
    fn prompt_includes_merge_summary_output() {
        let prompt = build_prompt(&sample_context(), false);
        assert!(prompt.contains("Merge Summary"));
    }

    #[test]
    fn prompt_skips_issue_close_in_multi_repo() {
        let mut ctx = sample_context();
        ctx.target_repo = Some("backend-api".to_string());
        let prompt = build_prompt(&ctx, true);
        // Should still merge the PR
        assert!(prompt.contains("gh pr merge 99"));
        // But should NOT try to close the issue (executor handles it)
        assert!(!prompt.contains("gh issue close"));
    }

    #[test]
    fn prompt_includes_issue_close_in_single_repo() {
        let prompt = build_prompt(&sample_context(), true);
        assert!(prompt.contains("gh issue close 42"));
    }

    #[test]
    fn prompt_skips_issue_close_for_local_source() {
        let mut ctx = sample_context();
        ctx.issue_source = "local".to_string();
        let prompt = build_prompt(&ctx, true);
        assert!(prompt.contains("gh pr merge 99"));
        assert!(!prompt.contains("gh issue close"));
    }

    #[test]
    fn prompt_uses_local_issue_reference_for_local_source() {
        let mut ctx = sample_context();
        ctx.issue_source = "local".to_string();
        let prompt = build_prompt(&ctx, true);
        assert!(prompt.contains("From local issue #42"));
        assert!(!prompt.contains("Resolves #42"));
    }
}
