use anyhow::{Context, Result};
use askama::Template;

use super::AgentContext;

#[derive(Template)]
#[template(path = "merger.txt")]
struct MergerPrompt<'a> {
    ctx: &'a AgentContext,
    auto_merge: bool,
    pr_number: u32,
    safe_title: String,
}

/// Escape shell metacharacters in a string for safe embedding in double-quoted
/// shell commands. Also strips control characters (newlines, tabs, etc.) that
/// could break command structure.
fn shell_escape(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '"' | '\\' | '$' | '`' | '!' | '\'' => {
                let mut escaped = String::with_capacity(2);
                escaped.push('\\');
                escaped.push(c);
                Some(escaped)
            }
            '\n' | '\r' | '\0' => None,
            c if c.is_control() => None,
            _ => Some(c.to_string()),
        })
        .collect()
}

pub fn build_prompt(ctx: &AgentContext, auto_merge: bool) -> Result<String> {
    let pr_number = ctx.pr_number.context("merger requires a PR number")?;
    let safe_title = shell_escape(&ctx.issue_title);
    let tmpl = MergerPrompt { ctx, auto_merge, pr_number, safe_title };
    tmpl.render().context("rendering merger template")
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
        let prompt = build_prompt(&sample_context(), false).unwrap();
        assert!(prompt.contains("PR #99"));
        assert!(prompt.contains("gh pr ready 99"));
        assert!(prompt.contains("#42"));
    }

    #[test]
    fn prompt_without_merge() {
        let prompt = build_prompt(&sample_context(), false).unwrap();
        assert!(prompt.contains("gh pr ready 99"));
        assert!(!prompt.contains("gh pr merge"));
    }

    #[test]
    fn prompt_with_merge() {
        let prompt = build_prompt(&sample_context(), true).unwrap();
        assert!(prompt.contains("gh pr ready 99"));
        assert!(prompt.contains("gh pr merge 99"));
        assert!(prompt.contains("--squash"));
        assert!(prompt.contains("--delete-branch"));
    }

    #[test]
    fn prompt_includes_issue_close_when_auto_merge() {
        let prompt = build_prompt(&sample_context(), true).unwrap();
        assert!(prompt.contains("gh issue close 42"));
    }

    #[test]
    fn prompt_includes_pr_description_update() {
        let prompt = build_prompt(&sample_context(), false).unwrap();
        assert!(prompt.contains("gh pr edit 99"));
        assert!(prompt.contains("Resolves #42"));
    }

    #[test]
    fn prompt_includes_merge_summary_output() {
        let prompt = build_prompt(&sample_context(), false).unwrap();
        assert!(prompt.contains("Merge Summary"));
    }

    #[test]
    fn prompt_skips_issue_close_in_multi_repo() {
        let mut ctx = sample_context();
        ctx.target_repo = Some("backend-api".to_string());
        let prompt = build_prompt(&ctx, true).unwrap();
        // Should still merge the PR
        assert!(prompt.contains("gh pr merge 99"));
        // But should NOT try to close the issue (executor handles it)
        assert!(!prompt.contains("gh issue close"));
    }

    #[test]
    fn prompt_includes_issue_close_in_single_repo() {
        let prompt = build_prompt(&sample_context(), true).unwrap();
        assert!(prompt.contains("gh issue close 42"));
    }

    #[test]
    fn prompt_skips_issue_close_for_local_source() {
        let mut ctx = sample_context();
        ctx.issue_source = "local".to_string();
        let prompt = build_prompt(&ctx, true).unwrap();
        assert!(prompt.contains("gh pr merge 99"));
        assert!(!prompt.contains("gh issue close"));
    }

    #[test]
    fn prompt_uses_local_issue_reference_for_local_source() {
        let mut ctx = sample_context();
        ctx.issue_source = "local".to_string();
        let prompt = build_prompt(&ctx, true).unwrap();
        assert!(prompt.contains("From local issue #42"));
        assert!(!prompt.contains("Resolves #42"));
    }

    #[test]
    fn prompt_fails_without_pr_number() {
        let mut ctx = sample_context();
        ctx.pr_number = None;
        let result = build_prompt(&ctx, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("PR number"));
    }

    #[test]
    fn prompt_escapes_shell_metacharacters_in_title() {
        let mut ctx = sample_context();
        ctx.issue_title = r#"Fix "bug" with $HOME expansion"#.to_string();
        let prompt = build_prompt(&ctx, false).unwrap();
        assert!(prompt.contains(r#"Fix \"bug\" with \$HOME expansion"#));
    }

    #[test]
    fn shell_escape_strips_newlines() {
        assert_eq!(shell_escape("line1\nline2\rline3"), "line1line2line3");
    }

    #[test]
    fn shell_escape_strips_null_bytes() {
        assert_eq!(shell_escape("before\0after"), "beforeafter");
    }

    #[test]
    fn shell_escape_escapes_single_quotes() {
        assert_eq!(shell_escape("it's"), r"it\'s");
    }
}
