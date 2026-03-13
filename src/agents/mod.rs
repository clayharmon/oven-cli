pub mod fixer;
pub mod implementer;
pub mod merger;
pub mod planner;
pub mod reviewer;

use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use crate::{db::ReviewFinding, process::CommandRunner};

/// The five agent roles in the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRole {
    Planner,
    Implementer,
    Reviewer,
    Fixer,
    Merger,
}

impl AgentRole {
    pub const fn allowed_tools(&self) -> &[&str] {
        match self {
            Self::Planner | Self::Reviewer => &["Read", "Glob", "Grep"],
            Self::Implementer | Self::Fixer => &["Read", "Write", "Edit", "Glob", "Grep", "Bash"],
            Self::Merger => &["Bash"],
        }
    }

    pub const fn as_str(&self) -> &str {
        match self {
            Self::Planner => "planner",
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Fixer => "fixer",
            Self::Merger => "merger",
        }
    }

    pub fn tools_as_strings(&self) -> Vec<String> {
        self.allowed_tools().iter().map(|s| (*s).to_string()).collect()
    }
}

impl std::fmt::Display for AgentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentRole {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "planner" => Ok(Self::Planner),
            "implementer" => Ok(Self::Implementer),
            "reviewer" => Ok(Self::Reviewer),
            "fixer" => Ok(Self::Fixer),
            "merger" => Ok(Self::Merger),
            other => anyhow::bail!("unknown agent role: {other}"),
        }
    }
}

/// Context passed to agent prompt builders.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub issue_number: u32,
    pub issue_title: String,
    pub issue_body: String,
    pub branch: String,
    pub pr_number: Option<u32>,
    pub test_command: Option<String>,
    pub lint_command: Option<String>,
    pub review_findings: Option<Vec<ReviewFinding>>,
    pub cycle: u32,
}

/// An invocation ready to be sent to the process runner.
pub struct AgentInvocation {
    pub role: AgentRole,
    pub prompt: String,
    pub working_dir: PathBuf,
}

/// Invoke an agent via the command runner.
pub async fn invoke_agent<R: CommandRunner>(
    runner: &R,
    invocation: &AgentInvocation,
) -> Result<crate::process::AgentResult> {
    runner
        .run_claude(
            &invocation.prompt,
            &invocation.role.tools_as_strings(),
            &invocation.working_dir,
        )
        .await
}

/// Structured output from the reviewer agent.
#[derive(Debug, Deserialize)]
pub struct ReviewOutput {
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub category: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub line_number: Option<u32>,
    pub message: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

/// Parse structured review output from the reviewer's text response.
///
/// The JSON may be wrapped in markdown code fences.
pub fn parse_review_output(text: &str) -> Result<ReviewOutput> {
    // Try direct JSON parse first
    if let Ok(output) = serde_json::from_str::<ReviewOutput>(text) {
        return Ok(output);
    }

    // Try extracting JSON from code fences
    if let Some(json_str) = extract_json_from_fences(text) {
        if let Ok(output) = serde_json::from_str::<ReviewOutput>(json_str) {
            return Ok(output);
        }
    }

    // Try finding JSON object in the text
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            let candidate = &text[start..=end];
            if let Ok(output) = serde_json::from_str::<ReviewOutput>(candidate) {
                return Ok(output);
            }
        }
    }

    // No findings found - return empty
    Ok(ReviewOutput { findings: Vec::new(), summary: String::new() })
}

fn extract_json_from_fences(text: &str) -> Option<&str> {
    let start_markers = ["```json\n", "```json\r\n", "```\n", "```\r\n"];
    for marker in &start_markers {
        if let Some(start) = text.find(marker) {
            let content_start = start + marker.len();
            if let Some(end) = text[content_start..].find("```") {
                return Some(&text[content_start..content_start + end]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_scoping_per_role() {
        assert_eq!(AgentRole::Planner.allowed_tools(), &["Read", "Glob", "Grep"]);
        assert_eq!(
            AgentRole::Implementer.allowed_tools(),
            &["Read", "Write", "Edit", "Glob", "Grep", "Bash"]
        );
        assert_eq!(AgentRole::Reviewer.allowed_tools(), &["Read", "Glob", "Grep"]);
        assert_eq!(
            AgentRole::Fixer.allowed_tools(),
            &["Read", "Write", "Edit", "Glob", "Grep", "Bash"]
        );
        assert_eq!(AgentRole::Merger.allowed_tools(), &["Bash"]);
    }

    #[test]
    fn role_display_roundtrip() {
        let roles = [
            AgentRole::Planner,
            AgentRole::Implementer,
            AgentRole::Reviewer,
            AgentRole::Fixer,
            AgentRole::Merger,
        ];
        for role in roles {
            let s = role.to_string();
            let parsed: AgentRole = s.parse().unwrap();
            assert_eq!(role, parsed);
        }
    }

    #[test]
    fn parse_review_output_valid_json() {
        let json = r#"{"findings":[{"severity":"critical","category":"bug","file_path":"src/main.rs","line_number":10,"message":"null pointer"}],"summary":"one issue found"}"#;
        let output = parse_review_output(json).unwrap();
        assert_eq!(output.findings.len(), 1);
        assert_eq!(output.findings[0].severity, Severity::Critical);
        assert_eq!(output.findings[0].message, "null pointer");
        assert_eq!(output.summary, "one issue found");
    }

    #[test]
    fn parse_review_output_in_code_fences() {
        let text = r#"Here are my findings:

```json
{"findings":[{"severity":"warning","category":"style","message":"missing docs"}],"summary":"ok"}
```

That's it."#;
        let output = parse_review_output(text).unwrap();
        assert_eq!(output.findings.len(), 1);
        assert_eq!(output.findings[0].severity, Severity::Warning);
    }

    #[test]
    fn parse_review_output_embedded_json() {
        let text = r#"I reviewed the code and found: {"findings":[{"severity":"info","category":"note","message":"looks fine"}],"summary":"clean"} end of review"#;
        let output = parse_review_output(text).unwrap();
        assert_eq!(output.findings.len(), 1);
    }

    #[test]
    fn parse_review_output_no_json() {
        let text = "The code looks great, no issues found.";
        let output = parse_review_output(text).unwrap();
        assert!(output.findings.is_empty());
    }

    #[test]
    fn parse_review_output_malformed_json() {
        let text = r#"{"findings": [{"broken json"#;
        let output = parse_review_output(text).unwrap();
        assert!(output.findings.is_empty());
    }
}
