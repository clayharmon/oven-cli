pub mod fixer;
pub mod implementer;
pub mod merger;
pub mod planner;
pub mod reviewer;

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, de::DeserializeOwned};

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
    /// When set, indicates this is a multi-repo pipeline where the PR lives in a
    /// different repo than the issue. The merger should skip closing the issue
    /// (the executor handles it).
    pub target_repo: Option<String>,
    /// Issue source: "github" or "local". The merger skips `gh issue close`
    /// for local issues since they're not on GitHub.
    pub issue_source: String,
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

/// Complexity classification from the planner agent.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    Simple,
    Full,
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Simple => "simple",
            Self::Full => "full",
        })
    }
}

impl std::str::FromStr for Complexity {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "simple" => Ok(Self::Simple),
            "full" => Ok(Self::Full),
            other => anyhow::bail!("unknown complexity: {other}"),
        }
    }
}

/// Structured output from the planner agent.
#[derive(Debug, Deserialize)]
pub struct PlannerOutput {
    pub batches: Vec<Batch>,
    #[serde(default)]
    pub total_issues: u32,
    #[serde(default)]
    pub parallel_capacity: u32,
}

#[derive(Debug, Deserialize)]
pub struct Batch {
    pub batch: u32,
    pub issues: Vec<PlannedIssue>,
    #[serde(default)]
    pub reasoning: String,
}

#[derive(Debug, Deserialize)]
pub struct PlannedIssue {
    pub number: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub predicted_files: Vec<String>,
    #[serde(default)]
    pub has_migration: bool,
    #[serde(default = "default_full")]
    pub complexity: Complexity,
}

const fn default_full() -> Complexity {
    Complexity::Full
}

/// Parse structured planner output from the planner's text response.
///
/// Falls back to `None` if the output is unparseable.
pub fn parse_planner_output(text: &str) -> Option<PlannerOutput> {
    extract_json(text)
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

impl Severity {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse structured review output from the reviewer's text response.
///
/// The JSON may be wrapped in markdown code fences. Returns empty findings
/// if the output is unparseable.
pub fn parse_review_output(text: &str) -> Result<ReviewOutput> {
    Ok(extract_json(text).unwrap_or(ReviewOutput { findings: Vec::new(), summary: String::new() }))
}

/// Try to extract a JSON object of type `T` from text that may contain prose,
/// code fences, or raw JSON.
///
/// Attempts three strategies in order:
/// 1. Direct `serde_json::from_str`
/// 2. JSON inside markdown code fences
/// 3. First `{` to last `}` in the text
fn extract_json<T: DeserializeOwned>(text: &str) -> Option<T> {
    if let Ok(val) = serde_json::from_str::<T>(text) {
        return Some(val);
    }

    if let Some(json_str) = extract_json_from_fences(text) {
        if let Ok(val) = serde_json::from_str::<T>(json_str) {
            return Some(val);
        }
    }

    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start { serde_json::from_str::<T>(&text[start..=end]).ok() } else { None }
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
    use proptest::prelude::*;

    use super::*;

    const ALL_ROLES: [AgentRole; 5] = [
        AgentRole::Planner,
        AgentRole::Implementer,
        AgentRole::Reviewer,
        AgentRole::Fixer,
        AgentRole::Merger,
    ];

    proptest! {
        #[test]
        fn agent_role_display_fromstr_roundtrip(idx in 0..5usize) {
            let role = ALL_ROLES[idx];
            let s = role.to_string();
            let parsed: AgentRole = s.parse().unwrap();
            assert_eq!(role, parsed);
        }

        #[test]
        fn arbitrary_strings_never_panic_on_role_parse(s in "\\PC{1,50}") {
            let _ = s.parse::<AgentRole>();
        }

        #[test]
        fn parse_review_output_never_panics(text in "\\PC{0,500}") {
            // parse_review_output should never panic on any input
            let result = parse_review_output(&text);
            assert!(result.is_ok());
        }

        #[test]
        fn valid_review_json_always_parses(
            severity in prop_oneof!["critical", "warning", "info"],
            category in "[a-z]{3,15}",
            message in "[a-zA-Z0-9 ]{1,50}",
        ) {
            let json = format!(
                r#"{{"findings":[{{"severity":"{severity}","category":"{category}","message":"{message}"}}],"summary":"test"}}"#
            );
            let output = parse_review_output(&json).unwrap();
            assert_eq!(output.findings.len(), 1);
            assert_eq!(output.findings[0].category, category);
        }

        #[test]
        fn review_json_in_fences_parses(
            severity in prop_oneof!["critical", "warning", "info"],
            category in "[a-z]{3,15}",
            message in "[a-zA-Z0-9 ]{1,50}",
            prefix in "[a-zA-Z ]{0,30}",
            suffix in "[a-zA-Z ]{0,30}",
        ) {
            let json = format!(
                r#"{{"findings":[{{"severity":"{severity}","category":"{category}","message":"{message}"}}],"summary":"ok"}}"#
            );
            let text = format!("{prefix}\n```json\n{json}\n```\n{suffix}");
            let output = parse_review_output(&text).unwrap();
            assert_eq!(output.findings.len(), 1);
        }
    }

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

    // --- Planner output parsing tests ---

    #[test]
    fn parse_planner_output_valid_json() {
        let json = r#"{
            "batches": [{
                "batch": 1,
                "issues": [{
                    "number": 42,
                    "title": "Add login",
                    "area": "auth",
                    "predicted_files": ["src/auth.rs"],
                    "has_migration": false,
                    "complexity": "simple"
                }],
                "reasoning": "standalone issue"
            }],
            "total_issues": 1,
            "parallel_capacity": 1
        }"#;
        let output = parse_planner_output(json).unwrap();
        assert_eq!(output.batches.len(), 1);
        assert_eq!(output.batches[0].issues.len(), 1);
        assert_eq!(output.batches[0].issues[0].number, 42);
        assert_eq!(output.batches[0].issues[0].complexity, Complexity::Simple);
        assert!(!output.batches[0].issues[0].has_migration);
    }

    #[test]
    fn parse_planner_output_in_code_fences() {
        let text = r#"Here's the plan:

```json
{
    "batches": [{"batch": 1, "issues": [{"number": 1, "complexity": "full"}], "reasoning": "ok"}],
    "total_issues": 1,
    "parallel_capacity": 1
}
```

That's the plan."#;
        let output = parse_planner_output(text).unwrap();
        assert_eq!(output.batches.len(), 1);
        assert_eq!(output.batches[0].issues[0].complexity, Complexity::Full);
    }

    #[test]
    fn parse_planner_output_malformed_returns_none() {
        assert!(parse_planner_output("not json at all").is_none());
        assert!(parse_planner_output(r#"{"batches": "broken"}"#).is_none());
        assert!(parse_planner_output("").is_none());
    }

    #[test]
    fn complexity_deserializes_from_strings() {
        let simple: Complexity = serde_json::from_str(r#""simple""#).unwrap();
        assert_eq!(simple, Complexity::Simple);
        let full: Complexity = serde_json::from_str(r#""full""#).unwrap();
        assert_eq!(full, Complexity::Full);
    }

    #[test]
    fn complexity_display_roundtrip() {
        for c in [Complexity::Simple, Complexity::Full] {
            let s = c.to_string();
            let parsed: Complexity = s.parse().unwrap();
            assert_eq!(c, parsed);
        }
    }

    #[test]
    fn planner_output_defaults_complexity_to_full() {
        let json = r#"{"batches": [{"batch": 1, "issues": [{"number": 5}], "reasoning": ""}], "total_issues": 1, "parallel_capacity": 1}"#;
        let output = parse_planner_output(json).unwrap();
        assert_eq!(output.batches[0].issues[0].complexity, Complexity::Full);
    }

    #[test]
    fn planner_output_with_multiple_batches() {
        let json = r#"{
            "batches": [
                {"batch": 1, "issues": [{"number": 1, "complexity": "simple"}, {"number": 2, "complexity": "simple"}], "reasoning": "independent"},
                {"batch": 2, "issues": [{"number": 3, "complexity": "full"}], "reasoning": "depends on batch 1"}
            ],
            "total_issues": 3,
            "parallel_capacity": 2
        }"#;
        let output = parse_planner_output(json).unwrap();
        assert_eq!(output.batches.len(), 2);
        assert_eq!(output.batches[0].issues.len(), 2);
        assert_eq!(output.batches[1].issues.len(), 1);
        assert_eq!(output.total_issues, 3);
    }
}
