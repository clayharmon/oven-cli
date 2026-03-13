use std::path::Path;

use anyhow::{Context, Result};

use super::{GlobalOpts, PrepArgs};
use crate::config::Config;

/// Embedded agent prompts for scaffolding into .claude/agents/.
const AGENT_PROMPTS: &[(&str, &str)] = &[
    ("implementer.md", include_str!("../../templates/implementer.txt")),
    ("reviewer.md", include_str!("../../templates/reviewer.txt")),
    ("fixer.md", include_str!("../../templates/fixer.txt")),
    ("planner.md", include_str!("../../templates/planner.txt")),
    ("merger.md", include_str!("../../templates/merger.txt")),
];

/// Embedded skill templates for scaffolding into .claude/skills/<name>/SKILL.md.
const SKILL_TEMPLATES: &[(&str, &str, &str)] = &[
    ("cook", "SKILL.md", include_str!("../../templates/skills/cook.md")),
    ("refine", "SKILL.md", include_str!("../../templates/skills/refine.md")),
];

#[allow(clippy::unused_async)]
pub async fn run(args: PrepArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;

    // User-level config (~/.config/oven/recipe.toml) - never overwritten, even with --force
    if let Some(config_dir) = dirs::config_dir() {
        let user_config_dir = config_dir.join("oven");
        let user_config_path = user_config_dir.join("recipe.toml");
        if user_config_path.exists() {
            println!("  ~/.config/oven/recipe.toml (exists, skipped)");
        } else {
            std::fs::create_dir_all(&user_config_dir)
                .with_context(|| format!("creating {}", user_config_dir.display()))?;
            std::fs::write(&user_config_path, Config::default_user_toml())
                .with_context(|| format!("writing {}", user_config_path.display()))?;
            println!("  ~/.config/oven/recipe.toml");
        }
    }

    // recipe.toml
    write_if_new_or_forced(
        &project_dir.join("recipe.toml"),
        &Config::default_project_toml(),
        args.force,
        "recipe.toml",
    )?;

    // .oven/ directories
    for sub in ["", "logs", "worktrees", "issues"] {
        let dir = project_dir.join(".oven").join(sub);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    // Initialize database
    let db_path = project_dir.join(".oven").join("oven.db");
    crate::db::open(&db_path)?;
    println!("  .oven/oven.db");

    // .claude/agents/
    let agents_dir = project_dir.join(".claude").join("agents");
    std::fs::create_dir_all(&agents_dir).context("creating .claude/agents/")?;

    for (filename, content) in AGENT_PROMPTS {
        write_if_new_or_forced(
            &agents_dir.join(filename),
            content,
            args.force,
            &format!(".claude/agents/{filename}"),
        )?;
    }

    // .claude/skills/
    for (skill_name, filename, content) in SKILL_TEMPLATES {
        let skill_dir = project_dir.join(".claude").join("skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir)
            .with_context(|| format!("creating .claude/skills/{skill_name}/"))?;
        write_if_new_or_forced(
            &skill_dir.join(filename),
            content,
            args.force,
            &format!(".claude/skills/{skill_name}/{filename}"),
        )?;
    }

    // .gitignore additions
    ensure_gitignore(&project_dir)?;

    println!("project ready");
    Ok(())
}

fn write_if_new_or_forced(path: &Path, content: &str, force: bool, label: &str) -> Result<()> {
    if path.exists() && !force {
        println!("  {label} (exists, skipped)");
        return Ok(());
    }
    let overwriting = path.exists();
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    if overwriting {
        println!("  {label} (overwritten)");
    } else {
        println!("  {label}");
    }
    Ok(())
}

fn ensure_gitignore(project_dir: &Path) -> Result<()> {
    let gitignore_path = project_dir.join(".gitignore");
    let entries = [".oven/"];

    let existing = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path).context("reading .gitignore")?
    } else {
        String::new()
    };

    let mut to_add = Vec::new();
    for entry in &entries {
        if !existing.lines().any(|line| line.trim() == *entry) {
            to_add.push(*entry);
        }
    }

    if !to_add.is_empty() {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        for entry in &to_add {
            content.push_str(entry);
            content.push('\n');
        }
        std::fs::write(&gitignore_path, content).context("writing .gitignore")?;
        println!("  .gitignore (updated)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_if_new_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        write_if_new_or_forced(&path, "hello", false, "test").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_if_new_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "original").unwrap();
        write_if_new_or_forced(&path, "new", false, "test").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn write_if_new_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "original").unwrap();
        write_if_new_or_forced(&path, "new", true, "test").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn ensure_gitignore_adds_entries() {
        let dir = tempfile::tempdir().unwrap();
        ensure_gitignore(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".oven/"));
    }

    #[test]
    fn ensure_gitignore_doesnt_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), ".oven/\n").unwrap();
        ensure_gitignore(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches(".oven/").count(), 1);
    }

    #[test]
    fn user_config_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let oven_dir = dir.path().join("oven");
        std::fs::create_dir_all(&oven_dir).unwrap();
        let config_path = oven_dir.join("recipe.toml");
        std::fs::write(&config_path, "# my custom config\n").unwrap();

        // User config uses direct exists() check, ignoring --force.
        assert!(config_path.exists());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), "# my custom config\n");
    }

    #[test]
    fn agent_prompts_are_embedded() {
        assert_eq!(AGENT_PROMPTS.len(), 5);
        for (name, content) in AGENT_PROMPTS {
            assert!(!name.is_empty());
            assert!(!content.is_empty());
        }
    }

    #[test]
    fn cook_skill_template_is_embeddable() {
        let content = include_str!("../../templates/skills/cook.md");
        assert!(!content.is_empty());
        assert!(content.contains("name: cook"));
        assert!(content.contains("Phase 1"));
        assert!(content.contains("Phase 2"));
        assert!(content.contains("Phase 3"));
        assert!(content.contains("Phase 4"));
        assert!(content.contains("Acceptance Criteria"));
        assert!(content.contains("Implementation Guide"));
        assert!(content.contains("Security Considerations"));
        assert!(content.contains("Test Requirements"));
        assert!(content.contains("Out of Scope"));
        assert!(content.contains("Dependencies"));
    }

    #[test]
    fn skill_templates_are_embedded() {
        assert_eq!(SKILL_TEMPLATES.len(), 2);
        for (name, filename, content) in SKILL_TEMPLATES {
            assert!(!name.is_empty());
            assert!(!filename.is_empty());
            assert!(!content.is_empty());
        }
    }

    #[test]
    fn refine_skill_template_is_embeddable() {
        let content = include_str!("../../templates/skills/refine.md");
        assert!(!content.is_empty());
        assert!(content.contains("name: refine"));
        // All six analysis dimensions
        assert!(content.contains("### Security"));
        assert!(content.contains("### Error Handling"));
        assert!(content.contains("### Bad Patterns"));
        assert!(content.contains("### Test Gaps"));
        assert!(content.contains("### Data Issues"));
        assert!(content.contains("### Dependencies"));
        // Report format with severity table
        assert!(content.contains("| Category | Critical | High | Medium | Low |"));
        // Phase 5 connects to /cook
        assert!(content.contains("/cook"));
    }
}
