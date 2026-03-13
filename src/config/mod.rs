use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub project: ProjectConfig,
    pub pipeline: PipelineConfig,
    pub labels: LabelConfig,
    #[serde(default)]
    pub repos: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProjectConfig {
    pub name: Option<String>,
    pub test: Option<String>,
    pub lint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PipelineConfig {
    pub max_parallel: u32,
    pub cost_budget: f64,
    pub poll_interval: u64,
    pub turn_limit: u32,
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
        Self { max_parallel: 2, cost_budget: 15.0, poll_interval: 60, turn_limit: 50 }
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
    repos: Option<HashMap<String, PathBuf>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawProjectConfig {
    name: Option<String>,
    test: Option<String>,
    lint: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPipelineConfig {
    max_parallel: Option<u32>,
    cost_budget: Option<f64>,
    poll_interval: Option<u64>,
    turn_limit: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLabelConfig {
    ready: Option<String>,
    cooking: Option<String>,
    complete: Option<String>,
    failed: Option<String>,
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

        Ok(config)
    }

    /// Generate a starter project TOML for `oven prep`.
    pub fn default_project_toml() -> String {
        r#"[project]
# name = "my-project"    # auto-detected from git remote
# test = "cargo test"    # test command
# lint = "cargo clippy"  # lint command

[pipeline]
max_parallel = 2
cost_budget = 15.0
poll_interval = 60

# [labels]
# ready = "o-ready"
# cooking = "o-cooking"
# complete = "o-complete"
# failed = "o-failed"
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
    use super::*;

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
            },
            pipeline: PipelineConfig { max_parallel: 5, cost_budget: 25.0, ..Default::default() },
            labels: LabelConfig::default(),
            repos: HashMap::from([("svc".to_string(), PathBuf::from("/tmp/svc"))]),
        };
        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config, deserialized);
    }
}
