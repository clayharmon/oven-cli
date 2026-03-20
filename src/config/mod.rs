use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Where oven reads issues from: GitHub API or local `.oven/issues/` files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IssueSource {
    #[default]
    Github,
    Local,
}

/// How PRs are merged into the base branch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    Squash,
    #[default]
    Merge,
    Rebase,
}

impl MergeStrategy {
    /// Return the `gh pr merge` CLI flag for this strategy.
    pub const fn gh_flag(&self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub project: ProjectConfig,
    pub pipeline: PipelineConfig,
    pub labels: LabelConfig,
    pub multi_repo: MultiRepoConfig,
    pub models: ModelConfig,
    #[serde(default)]
    pub repos: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MultiRepoConfig {
    pub enabled: bool,
    pub target_field: String,
}

impl Default for MultiRepoConfig {
    fn default() -> Self {
        Self { enabled: false, target_field: "target_repo".to_string() }
    }
}

/// Per-agent model overrides.
///
/// `default` applies to any agent without an explicit override. Values are passed
/// directly as the `--model` flag to the claude CLI (e.g. "opus", "sonnet").
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelConfig {
    pub default: Option<String>,
    pub planner: Option<String>,
    pub implementer: Option<String>,
    pub reviewer: Option<String>,
    pub fixer: Option<String>,
}

impl ModelConfig {
    /// Get the model for a given agent role, falling back to `default`.
    /// Returns `None` if neither the agent nor default is set (use CLI default).
    pub fn model_for(&self, role: &str) -> Option<&str> {
        let agent_override = match role {
            "planner" => self.planner.as_deref(),
            "implementer" => self.implementer.as_deref(),
            "reviewer" => self.reviewer.as_deref(),
            "fixer" => self.fixer.as_deref(),
            _ => None,
        };
        agent_override.or(self.default.as_deref())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProjectConfig {
    pub name: Option<String>,
    pub test: Option<String>,
    pub lint: Option<String>,
    pub issue_source: IssueSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PipelineConfig {
    pub max_parallel: u32,
    pub cost_budget: f64,
    pub poll_interval: u64,
    pub turn_limit: u32,
    pub merge_strategy: MergeStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LabelConfig {
    pub ready: String,
    pub cooking: String,
    pub complete: String,
    pub failed: String,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_parallel: 2,
            cost_budget: 15.0,
            poll_interval: 60,
            turn_limit: 50,
            merge_strategy: MergeStrategy::default(),
        }
    }
}

impl Default for LabelConfig {
    fn default() -> Self {
        Self {
            ready: "o-ready".to_string(),
            cooking: "o-cooking".to_string(),
            complete: "o-complete".to_string(),
            failed: "o-failed".to_string(),
        }
    }
}

/// Intermediate representation for partial config deserialization.
/// All fields are optional so we can tell which ones were explicitly set.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    project: Option<RawProjectConfig>,
    pipeline: Option<RawPipelineConfig>,
    labels: Option<RawLabelConfig>,
    multi_repo: Option<RawMultiRepoConfig>,
    models: Option<RawModelConfig>,
    repos: Option<HashMap<String, PathBuf>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawProjectConfig {
    name: Option<String>,
    test: Option<String>,
    lint: Option<String>,
    issue_source: Option<IssueSource>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPipelineConfig {
    max_parallel: Option<u32>,
    cost_budget: Option<f64>,
    poll_interval: Option<u64>,
    turn_limit: Option<u32>,
    merge_strategy: Option<MergeStrategy>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLabelConfig {
    ready: Option<String>,
    cooking: Option<String>,
    complete: Option<String>,
    failed: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMultiRepoConfig {
    enabled: Option<bool>,
    target_field: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawModelConfig {
    default: Option<String>,
    planner: Option<String>,
    implementer: Option<String>,
    reviewer: Option<String>,
    fixer: Option<String>,
}

impl Config {
    /// Load config by merging user defaults with project overrides.
    ///
    /// User config: `~/.config/oven/recipe.toml`
    /// Project config: `recipe.toml` in `project_dir`
    ///
    /// Missing files are not errors - defaults are used instead.
    pub fn load(project_dir: &Path) -> anyhow::Result<Self> {
        let mut config = Self::default();

        // Load user config
        if let Some(config_dir) = dirs::config_dir() {
            let user_path = config_dir.join("oven").join("recipe.toml");
            if user_path.exists() {
                let content = std::fs::read_to_string(&user_path)
                    .with_context(|| format!("reading user config: {}", user_path.display()))?;
                let raw: RawConfig = toml::from_str(&content)
                    .with_context(|| format!("parsing user config: {}", user_path.display()))?;
                apply_raw(&mut config, &raw, true);
            }
        }

        // Load project config (overrides user config)
        let project_path = project_dir.join("recipe.toml");
        if project_path.exists() {
            let content = std::fs::read_to_string(&project_path)
                .with_context(|| format!("reading project config: {}", project_path.display()))?;
            let raw: RawConfig = toml::from_str(&content)
                .with_context(|| format!("parsing project config: {}", project_path.display()))?;
            apply_raw(&mut config, &raw, false);
        }

        config.validate()?;
        Ok(config)
    }

    /// Resolve a repo name to a local path.
    ///
    /// Returns an error if the repo name is not in the config or the path doesn't exist.
    pub fn resolve_repo(&self, name: &str) -> anyhow::Result<PathBuf> {
        let path = self
            .repos
            .get(name)
            .with_context(|| format!("repo '{name}' not found in user config [repos] section"))?;

        let expanded = if path.starts_with("~") {
            dirs::home_dir().map_or_else(
                || path.clone(),
                |home| home.join(path.strip_prefix("~").unwrap_or(path)),
            )
        } else {
            path.clone()
        };

        if !expanded.exists() {
            anyhow::bail!("repo '{name}' path does not exist: {}", expanded.display());
        }

        Ok(expanded)
    }

    /// Validate config values that could cause hangs or resource exhaustion.
    fn validate(&self) -> anyhow::Result<()> {
        if self.pipeline.max_parallel == 0 {
            anyhow::bail!("pipeline.max_parallel must be >= 1 (got 0, which would deadlock)");
        }
        if self.pipeline.poll_interval < 10 {
            anyhow::bail!(
                "pipeline.poll_interval must be >= 10 (got {}, which would hammer the API)",
                self.pipeline.poll_interval
            );
        }
        if !self.pipeline.cost_budget.is_finite() || self.pipeline.cost_budget <= 0.0 {
            anyhow::bail!(
                "pipeline.cost_budget must be a finite number > 0 (got {})",
                self.pipeline.cost_budget
            );
        }
        if self.pipeline.turn_limit == 0 {
            anyhow::bail!("pipeline.turn_limit must be >= 1 (got 0)");
        }
        Ok(())
    }

    /// Generate a starter user TOML for `~/.config/oven/recipe.toml`.
    pub fn default_user_toml() -> String {
        r#"# Global oven defaults (all projects inherit these)

[pipeline]
# max_parallel = 2
# cost_budget = 15.0
# poll_interval = 60
# turn_limit = 50
# merge_strategy = "merge"  # "merge" (default), "squash", or "rebase"

# [labels]
# ready = "o-ready"
# cooking = "o-cooking"
# complete = "o-complete"
# failed = "o-failed"

# Multi-repo path mappings (only honored from user config)
# [repos]
# api = "~/dev/api"
# web = "~/dev/web"
"#
        .to_string()
    }

    /// Generate a starter project TOML for `oven prep`.
    pub fn default_project_toml() -> String {
        r#"[project]
# name = "my-project"    # auto-detected from git remote
# test = "cargo test"    # test command
# lint = "cargo clippy"  # lint command
# issue_source = "github"  # "github" (default) or "local"

[pipeline]
max_parallel = 2
cost_budget = 15.0
poll_interval = 60
# merge_strategy = "merge"  # "merge" (default), "squash", or "rebase"

# [labels]
# ready = "o-ready"
# cooking = "o-cooking"
# complete = "o-complete"
# failed = "o-failed"

# [models]
# default = "sonnet"
# implementer = "opus"
# fixer = "opus"
"#
        .to_string()
    }
}

/// Apply a raw (partial) config onto the resolved config.
/// `allow_repos` controls whether the `repos` key is honored (only from user config).
fn apply_raw(config: &mut Config, raw: &RawConfig, allow_repos: bool) {
    if let Some(ref project) = raw.project {
        if project.name.is_some() {
            config.project.name.clone_from(&project.name);
        }
        if project.test.is_some() {
            config.project.test.clone_from(&project.test);
        }
        if project.lint.is_some() {
            config.project.lint.clone_from(&project.lint);
        }
        if let Some(ref source) = project.issue_source {
            config.project.issue_source = source.clone();
        }
    }

    if let Some(ref pipeline) = raw.pipeline {
        if let Some(v) = pipeline.max_parallel {
            config.pipeline.max_parallel = v;
        }
        if let Some(v) = pipeline.cost_budget {
            config.pipeline.cost_budget = v;
        }
        if let Some(v) = pipeline.poll_interval {
            config.pipeline.poll_interval = v;
        }
        if let Some(v) = pipeline.turn_limit {
            config.pipeline.turn_limit = v;
        }
        if let Some(ref v) = pipeline.merge_strategy {
            config.pipeline.merge_strategy = v.clone();
        }
    }

    if let Some(ref labels) = raw.labels {
        if let Some(ref v) = labels.ready {
            config.labels.ready.clone_from(v);
        }
        if let Some(ref v) = labels.cooking {
            config.labels.cooking.clone_from(v);
        }
        if let Some(ref v) = labels.complete {
            config.labels.complete.clone_from(v);
        }
        if let Some(ref v) = labels.failed {
            config.labels.failed.clone_from(v);
        }
    }

    // multi_repo settings from project config (controls feature enablement)
    if let Some(ref multi_repo) = raw.multi_repo {
        if let Some(v) = multi_repo.enabled {
            config.multi_repo.enabled = v;
        }
        if let Some(ref v) = multi_repo.target_field {
            config.multi_repo.target_field.clone_from(v);
        }
    }

    if let Some(ref models) = raw.models {
        if models.default.is_some() {
            config.models.default.clone_from(&models.default);
        }
        if models.planner.is_some() {
            config.models.planner.clone_from(&models.planner);
        }
        if models.implementer.is_some() {
            config.models.implementer.clone_from(&models.implementer);
        }
        if models.reviewer.is_some() {
            config.models.reviewer.clone_from(&models.reviewer);
        }
        if models.fixer.is_some() {
            config.models.fixer.clone_from(&models.fixer);
        }
    }

    // repos only honored from user config (security: project config shouldn't
    // be able to point the tool at arbitrary repos on the filesystem)
    if allow_repos {
        if let Some(ref repos) = raw.repos {
            config.repos.clone_from(repos);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn config_toml_roundtrip(
            max_parallel in 1..100u32,
            cost_budget in 0.0..1000.0f64,
            poll_interval in 1..3600u64,
            turn_limit in 1..200u32,
            ready in "[a-z][a-z0-9-]{1,20}",
            cooking in "[a-z][a-z0-9-]{1,20}",
            complete in "[a-z][a-z0-9-]{1,20}",
            failed in "[a-z][a-z0-9-]{1,20}",
        ) {
            let config = Config {
                project: ProjectConfig::default(),
                pipeline: PipelineConfig { max_parallel, cost_budget, poll_interval, turn_limit, ..Default::default() },
                labels: LabelConfig { ready, cooking, complete, failed },
                multi_repo: MultiRepoConfig::default(),
                models: ModelConfig::default(),
                repos: HashMap::new(),
            };
            let serialized = toml::to_string(&config).unwrap();
            let deserialized: Config = toml::from_str(&serialized).unwrap();
            assert_eq!(config.pipeline.max_parallel, deserialized.pipeline.max_parallel);
            assert!((config.pipeline.cost_budget - deserialized.pipeline.cost_budget).abs() < 1e-6);
            assert_eq!(config.pipeline.poll_interval, deserialized.pipeline.poll_interval);
            assert_eq!(config.pipeline.turn_limit, deserialized.pipeline.turn_limit);
            assert_eq!(config.labels, deserialized.labels);
        }

        #[test]
        fn partial_toml_always_parses(
            max_parallel in proptest::option::of(1..100u32),
            cost_budget in proptest::option::of(0.0..1000.0f64),
        ) {
            let mut parts = vec!["[pipeline]".to_string()];
            if let Some(mp) = max_parallel {
                parts.push(format!("max_parallel = {mp}"));
            }
            if let Some(cb) = cost_budget {
                parts.push(format!("cost_budget = {cb}"));
            }
            let toml_str = parts.join("\n");
            let raw: RawConfig = toml::from_str(&toml_str).unwrap();
            let mut config = Config::default();
            apply_raw(&mut config, &raw, false);
            if let Some(mp) = max_parallel {
                assert_eq!(config.pipeline.max_parallel, mp);
            }
        }
    }

    #[test]
    fn defaults_are_correct() {
        let config = Config::default();
        assert_eq!(config.pipeline.max_parallel, 2);
        assert!(
            (config.pipeline.cost_budget - 15.0).abs() < f64::EPSILON,
            "cost_budget should be 15.0"
        );
        assert_eq!(config.pipeline.poll_interval, 60);
        assert_eq!(config.pipeline.turn_limit, 50);
        assert_eq!(config.labels.ready, "o-ready");
        assert_eq!(config.labels.cooking, "o-cooking");
        assert_eq!(config.labels.complete, "o-complete");
        assert_eq!(config.labels.failed, "o-failed");
        assert!(config.project.name.is_none());
        assert!(config.repos.is_empty());
        assert!(!config.multi_repo.enabled);
        assert_eq!(config.multi_repo.target_field, "target_repo");
    }

    #[test]
    fn load_from_valid_toml() {
        let toml_str = r#"
[project]
name = "test-project"
test = "cargo test"

[pipeline]
max_parallel = 4
cost_budget = 20.0
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);

        assert_eq!(config.project.name.as_deref(), Some("test-project"));
        assert_eq!(config.project.test.as_deref(), Some("cargo test"));
        assert_eq!(config.pipeline.max_parallel, 4);
        assert!((config.pipeline.cost_budget - 20.0).abs() < f64::EPSILON);
        // Unset fields keep defaults
        assert_eq!(config.pipeline.poll_interval, 60);
    }

    #[test]
    fn project_overrides_user() {
        let user_toml = r"
[pipeline]
max_parallel = 3
cost_budget = 10.0
poll_interval = 120
";
        let project_toml = r"
[pipeline]
max_parallel = 1
cost_budget = 5.0
";
        let mut config = Config::default();

        let user_raw: RawConfig = toml::from_str(user_toml).unwrap();
        apply_raw(&mut config, &user_raw, true);
        assert_eq!(config.pipeline.max_parallel, 3);
        assert_eq!(config.pipeline.poll_interval, 120);

        let project_raw: RawConfig = toml::from_str(project_toml).unwrap();
        apply_raw(&mut config, &project_raw, false);
        assert_eq!(config.pipeline.max_parallel, 1);
        assert!((config.pipeline.cost_budget - 5.0).abs() < f64::EPSILON);
        // poll_interval not overridden by project, stays at user value
        assert_eq!(config.pipeline.poll_interval, 120);
    }

    #[test]
    fn repos_ignored_in_project_config() {
        let project_toml = r#"
[repos]
evil = "/tmp/evil"
"#;
        let mut config = Config::default();
        let raw: RawConfig = toml::from_str(project_toml).unwrap();
        apply_raw(&mut config, &raw, false);
        assert!(config.repos.is_empty());
    }

    #[test]
    fn repos_honored_in_user_config() {
        let user_toml = r#"
[repos]
api = "/home/user/dev/api"
"#;
        let mut config = Config::default();
        let raw: RawConfig = toml::from_str(user_toml).unwrap();
        apply_raw(&mut config, &raw, true);
        assert_eq!(config.repos.get("api").unwrap(), Path::new("/home/user/dev/api"));
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("recipe.toml"), "this is not [valid toml").unwrap();
        let result = Config::load(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("parsing project config"), "error was: {err}");
    }

    #[test]
    fn default_user_toml_parses() {
        let toml_str = Config::default_user_toml();
        let raw: RawConfig = toml::from_str(&toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, true);
        // All commented out, so defaults remain
        assert_eq!(config.pipeline.max_parallel, 2);
        assert!(config.repos.is_empty());
    }

    #[test]
    fn default_project_toml_parses() {
        let toml_str = Config::default_project_toml();
        let raw: RawConfig = toml::from_str(&toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        // Should still have defaults since commented lines are ignored
        assert_eq!(config.pipeline.max_parallel, 2);
    }

    #[test]
    fn config_roundtrip_serialize_deserialize() {
        let config = Config {
            project: ProjectConfig {
                name: Some("roundtrip".to_string()),
                test: Some("make test".to_string()),
                lint: None,
                issue_source: IssueSource::Github,
            },
            pipeline: PipelineConfig { max_parallel: 5, cost_budget: 25.0, ..Default::default() },
            labels: LabelConfig::default(),
            multi_repo: MultiRepoConfig::default(),
            models: ModelConfig::default(),
            repos: HashMap::from([("svc".to_string(), PathBuf::from("/tmp/svc"))]),
        };
        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn multi_repo_config_from_project_toml() {
        let toml_str = r#"
[multi_repo]
enabled = true
target_field = "repo"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert!(config.multi_repo.enabled);
        assert_eq!(config.multi_repo.target_field, "repo");
    }

    #[test]
    fn multi_repo_defaults_when_not_specified() {
        let toml_str = r"
[pipeline]
max_parallel = 1
";
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert!(!config.multi_repo.enabled);
        assert_eq!(config.multi_repo.target_field, "target_repo");
    }

    #[test]
    fn resolve_repo_finds_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.repos.insert("test-repo".to_string(), dir.path().to_path_buf());

        let resolved = config.resolve_repo("test-repo").unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_repo_missing_name_errors() {
        let config = Config::default();
        let result = config.resolve_repo("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found in user config"));
    }

    #[test]
    fn resolve_repo_missing_path_errors() {
        let mut config = Config::default();
        config.repos.insert("bad".to_string(), PathBuf::from("/nonexistent/path/xyz"));
        let result = config.resolve_repo("bad");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn issue_source_defaults_to_github() {
        let config = Config::default();
        assert_eq!(config.project.issue_source, IssueSource::Github);
    }

    #[test]
    fn issue_source_local_parses() {
        let toml_str = r#"
[project]
issue_source = "local"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert_eq!(config.project.issue_source, IssueSource::Local);
    }

    #[test]
    fn issue_source_github_parses() {
        let toml_str = r#"
[project]
issue_source = "github"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert_eq!(config.project.issue_source, IssueSource::Github);
    }

    #[test]
    fn validate_rejects_zero_max_parallel() {
        let mut config = Config::default();
        config.pipeline.max_parallel = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_parallel"), "error was: {err}");
    }

    #[test]
    fn validate_rejects_low_poll_interval() {
        let mut config = Config::default();
        config.pipeline.poll_interval = 5;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("poll_interval"), "error was: {err}");
    }

    #[test]
    fn validate_rejects_zero_cost_budget() {
        let mut config = Config::default();
        config.pipeline.cost_budget = 0.0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("cost_budget"), "error was: {err}");
    }

    #[test]
    fn validate_rejects_nan_cost_budget() {
        let mut config = Config::default();
        config.pipeline.cost_budget = f64::NAN;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("cost_budget"), "error was: {err}");
    }

    #[test]
    fn validate_rejects_infinity_cost_budget() {
        let mut config = Config::default();
        config.pipeline.cost_budget = f64::INFINITY;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("cost_budget"), "error was: {err}");
    }

    #[test]
    fn validate_rejects_zero_turn_limit() {
        let mut config = Config::default();
        config.pipeline.turn_limit = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("turn_limit"), "error was: {err}");
    }

    #[test]
    fn validate_accepts_defaults() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn issue_source_invalid_errors() {
        let toml_str = r#"
[project]
issue_source = "jira"
"#;
        let result = toml::from_str::<RawConfig>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn issue_source_roundtrip() {
        let config = Config {
            project: ProjectConfig { issue_source: IssueSource::Local, ..Default::default() },
            ..Default::default()
        };
        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.project.issue_source, IssueSource::Local);
    }

    #[test]
    fn model_for_returns_agent_override() {
        let models = ModelConfig {
            default: Some("sonnet".to_string()),
            implementer: Some("opus".to_string()),
            ..Default::default()
        };
        assert_eq!(models.model_for("implementer"), Some("opus"));
        assert_eq!(models.model_for("reviewer"), Some("sonnet"));
    }

    #[test]
    fn model_for_returns_none_when_unset() {
        let models = ModelConfig::default();
        assert_eq!(models.model_for("planner"), None);
    }

    #[test]
    fn model_config_from_toml() {
        let toml_str = r#"
[models]
default = "sonnet"
implementer = "opus"
fixer = "opus"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert_eq!(config.models.default.as_deref(), Some("sonnet"));
        assert_eq!(config.models.implementer.as_deref(), Some("opus"));
        assert_eq!(config.models.fixer.as_deref(), Some("opus"));
        assert!(config.models.planner.is_none());
        assert!(config.models.reviewer.is_none());
    }

    #[test]
    fn model_config_project_overrides_user() {
        let user_toml = r#"
[models]
default = "sonnet"
implementer = "sonnet"
"#;
        let project_toml = r#"
[models]
implementer = "opus"
"#;
        let mut config = Config::default();
        let user_raw: RawConfig = toml::from_str(user_toml).unwrap();
        apply_raw(&mut config, &user_raw, true);
        assert_eq!(config.models.implementer.as_deref(), Some("sonnet"));

        let project_raw: RawConfig = toml::from_str(project_toml).unwrap();
        apply_raw(&mut config, &project_raw, false);
        assert_eq!(config.models.implementer.as_deref(), Some("opus"));
        // default stays from user config
        assert_eq!(config.models.default.as_deref(), Some("sonnet"));
    }

    #[test]
    fn model_config_defaults_when_not_specified() {
        let toml_str = r"
[pipeline]
max_parallel = 1
";
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert_eq!(config.models, ModelConfig::default());
    }

    #[test]
    fn merge_strategy_defaults_to_merge() {
        let config = Config::default();
        assert_eq!(config.pipeline.merge_strategy, MergeStrategy::Merge);
    }

    #[test]
    fn merge_strategy_squash_parses() {
        let toml_str = r#"
[pipeline]
merge_strategy = "squash"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert_eq!(config.pipeline.merge_strategy, MergeStrategy::Squash);
    }

    #[test]
    fn merge_strategy_rebase_parses() {
        let toml_str = r#"
[pipeline]
merge_strategy = "rebase"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        apply_raw(&mut config, &raw, false);
        assert_eq!(config.pipeline.merge_strategy, MergeStrategy::Rebase);
    }

    #[test]
    fn merge_strategy_invalid_errors() {
        let toml_str = r#"
[pipeline]
merge_strategy = "fast-forward"
"#;
        let result = toml::from_str::<RawConfig>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn merge_strategy_project_overrides_user() {
        let user_toml = r#"
[pipeline]
merge_strategy = "squash"
"#;
        let project_toml = r#"
[pipeline]
merge_strategy = "rebase"
"#;
        let mut config = Config::default();
        let user_raw: RawConfig = toml::from_str(user_toml).unwrap();
        apply_raw(&mut config, &user_raw, true);
        assert_eq!(config.pipeline.merge_strategy, MergeStrategy::Squash);

        let project_raw: RawConfig = toml::from_str(project_toml).unwrap();
        apply_raw(&mut config, &project_raw, false);
        assert_eq!(config.pipeline.merge_strategy, MergeStrategy::Rebase);
    }

    #[test]
    fn merge_strategy_gh_flags() {
        assert_eq!(MergeStrategy::Squash.gh_flag(), "--squash");
        assert_eq!(MergeStrategy::Merge.gh_flag(), "--merge");
        assert_eq!(MergeStrategy::Rebase.gh_flag(), "--rebase");
    }

    #[test]
    fn merge_strategy_roundtrip() {
        let config = Config {
            pipeline: PipelineConfig {
                merge_strategy: MergeStrategy::Rebase,
                ..Default::default()
            },
            ..Default::default()
        };
        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.pipeline.merge_strategy, MergeStrategy::Rebase);
    }
}
