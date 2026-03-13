use askama::Template;

#[derive(Template)]
#[template(path = "merger.txt")]
struct MergerPrompt {
    pr_number: u32,
    auto_merge: bool,
}

pub fn build_prompt(pr_number: u32, auto_merge: bool) -> String {
    let tmpl = MergerPrompt { pr_number, auto_merge };
    tmpl.render().expect("merger template render failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_without_merge() {
        let prompt = build_prompt(99, false);
        assert!(prompt.contains("gh pr ready 99"));
        assert!(!prompt.contains("gh pr merge"));
    }

    #[test]
    fn prompt_with_merge() {
        let prompt = build_prompt(99, true);
        assert!(prompt.contains("gh pr ready 99"));
        assert!(prompt.contains("gh pr merge 99"));
        assert!(prompt.contains("--squash"));
    }
}
