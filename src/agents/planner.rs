use askama::Template;

use crate::github::Issue;

#[derive(Template)]
#[template(path = "planner.txt")]
struct PlannerPrompt<'a> {
    issues: &'a [Issue],
}

pub fn build_prompt(issues: &[Issue]) -> String {
    let tmpl = PlannerPrompt { issues };
    tmpl.render().expect("planner template render failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Issue, IssueLabel};

    #[test]
    fn prompt_includes_issue_details() {
        let issues = vec![
            Issue {
                number: 1,
                title: "Add login".to_string(),
                body: "implement login flow".to_string(),
                labels: vec![IssueLabel { name: "o-ready".to_string() }],
            },
            Issue {
                number: 2,
                title: "Fix bug".to_string(),
                body: "crash on startup".to_string(),
                labels: vec![],
            },
        ];
        let prompt = build_prompt(&issues);
        assert!(prompt.contains("#1: Add login"));
        assert!(prompt.contains("#2: Fix bug"));
        assert!(prompt.contains("<issue_body>implement login flow</issue_body>"));
        assert!(prompt.contains("<issue_body>crash on startup</issue_body>"));
    }
}
