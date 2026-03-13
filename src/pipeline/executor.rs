use std::{fmt::Write as _, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    agents::{
        self, AgentContext, AgentInvocation, AgentRole, Complexity, PlannerOutput, Severity,
        invoke_agent, parse_planner_output, parse_review_output,
    },
    config::Config,
    db::{self, AgentRun, ReviewFinding, Run, RunStatus},
    git,
    github::{self, GhClient},
    issues::{IssueOrigin, IssueProvider, PipelineIssue},
    process::CommandRunner,
};

/// Runs a single issue through the full pipeline.
pub struct PipelineExecutor<R: CommandRunner> {
    pub runner: Arc<R>,
    pub github: Arc<GhClient<R>>,
    pub issues: Arc<dyn IssueProvider>,
    pub db: Arc<Mutex<Connection>>,
    pub config: Config,
    pub cancel_token: CancellationToken,
    pub repo_dir: PathBuf,
}

impl<R: CommandRunner + 'static> PipelineExecutor<R> {
    /// Run the full pipeline for a single issue.
    pub async fn run_issue(&self, issue: &PipelineIssue, auto_merge: bool) -> Result<()> {
        self.run_issue_with_complexity(issue, auto_merge, None).await
    }

    /// Run the full pipeline for a single issue with an optional complexity classification.
    pub async fn run_issue_with_complexity(
        &self,
        issue: &PipelineIssue,
        auto_merge: bool,
        complexity: Option<Complexity>,
    ) -> Result<()> {
        let run_id = generate_run_id();

        // Determine target repo for worktrees and PRs (multi-repo routing)
        let (target_dir, is_multi_repo) = self.resolve_target_dir(issue.target_repo.as_ref())?;

        let base_branch = git::default_branch(&target_dir).await?;

        let mut run = new_run(&run_id, issue, auto_merge);
        if let Some(ref c) = complexity {
            run.complexity = c.to_string();
        }
        {
            let conn = self.db.lock().await;
            db::runs::insert_run(&conn, &run)?;
        }

        self.issues
            .transition(issue.number, &self.config.labels.ready, &self.config.labels.cooking)
            .await?;

        let worktree = git::create_worktree(&target_dir, issue.number, &base_branch).await?;
        self.record_worktree(&run_id, &worktree).await?;

        info!(
            run_id = %run_id,
            issue = issue.number,
            branch = %worktree.branch,
            target_repo = ?issue.target_repo,
            "starting pipeline"
        );

        let issue_source_str = match issue.source {
            IssueOrigin::Github => "github",
            IssueOrigin::Local => "local",
        };

        let pr_number = self.create_pr(&run_id, issue, &worktree.branch, &target_dir).await?;

        let ctx = AgentContext {
            issue_number: issue.number,
            issue_title: issue.title.clone(),
            issue_body: issue.body.clone(),
            branch: worktree.branch.clone(),
            pr_number: Some(pr_number),
            test_command: self.config.project.test.clone(),
            lint_command: self.config.project.lint.clone(),
            review_findings: None,
            cycle: 1,
            target_repo: if is_multi_repo { issue.target_repo.clone() } else { None },
            issue_source: issue_source_str.to_string(),
        };

        let result = self.run_steps(&run_id, &ctx, &worktree.path, auto_merge).await;
        self.finalize_run(&run_id, issue, pr_number, &result).await?;

        if let Err(e) = git::remove_worktree(&target_dir, &worktree.path).await {
            warn!(run_id = %run_id, error = %e, "failed to clean up worktree");
        }

        result
    }

    /// Invoke the planner agent to decide batching and complexity for a set of issues.
    ///
    /// Returns `None` if the planner fails or returns unparseable output (fallback to default).
    pub async fn plan_issues(&self, issues: &[PipelineIssue]) -> Option<PlannerOutput> {
        let prompt = agents::planner::build_prompt(issues);
        let invocation = AgentInvocation {
            role: AgentRole::Planner,
            prompt,
            working_dir: self.repo_dir.clone(),
        };

        match invoke_agent(self.runner.as_ref(), &invocation).await {
            Ok(result) => {
                let parsed = parse_planner_output(&result.output);
                if parsed.is_none() {
                    warn!("planner returned unparseable output, falling back to single batch");
                }
                parsed
            }
            Err(e) => {
                warn!(error = %e, "planner agent failed, falling back to single batch");
                None
            }
        }
    }

    /// Determine the effective repo directory for worktrees and PRs.
    ///
    /// Returns `(target_dir, is_multi_repo)`. When multi-repo is disabled or no target
    /// is specified, falls back to `self.repo_dir`.
    fn resolve_target_dir(&self, target_repo: Option<&String>) -> Result<(PathBuf, bool)> {
        if !self.config.multi_repo.enabled {
            return Ok((self.repo_dir.clone(), false));
        }
        match target_repo {
            Some(name) => {
                let path = self.config.resolve_repo(name)?;
                Ok((path, true))
            }
            None => Ok((self.repo_dir.clone(), false)),
        }
    }

    async fn record_worktree(&self, run_id: &str, worktree: &git::Worktree) -> Result<()> {
        let conn = self.db.lock().await;
        conn.execute(
            "UPDATE runs SET branch = ?1, worktree_path = ?2 WHERE id = ?3",
            rusqlite::params![worktree.branch, worktree.path.to_string_lossy().as_ref(), run_id],
        )?;
        drop(conn);
        Ok(())
    }

    async fn create_pr(
        &self,
        run_id: &str,
        issue: &PipelineIssue,
        branch: &str,
        repo_dir: &std::path::Path,
    ) -> Result<u32> {
        let (pr_title, pr_body) = match issue.source {
            IssueOrigin::Github => (
                format!("fix(#{}): {}", issue.number, issue.title),
                format!(
                    "Resolves #{}\n\nAutomated by [oven](https://github.com/clayharmon/oven-cli).",
                    issue.number
                ),
            ),
            IssueOrigin::Local => (
                format!("fix: {}", issue.title),
                format!(
                    "From local issue #{}\n\nAutomated by [oven](https://github.com/clayharmon/oven-cli).",
                    issue.number
                ),
            ),
        };

        git::push_branch(repo_dir, branch).await?;
        let pr_number =
            self.github.create_draft_pr_in(&pr_title, branch, &pr_body, repo_dir).await?;

        {
            let conn = self.db.lock().await;
            db::runs::update_run_pr(&conn, run_id, pr_number)?;
        }

        info!(run_id = %run_id, pr = pr_number, "draft PR created");
        Ok(pr_number)
    }

    async fn finalize_run(
        &self,
        run_id: &str,
        issue: &PipelineIssue,
        pr_number: u32,
        result: &Result<()>,
    ) -> Result<()> {
        let (final_status, error_msg) = match result {
            Ok(()) => {
                self.issues
                    .transition(
                        issue.number,
                        &self.config.labels.cooking,
                        &self.config.labels.complete,
                    )
                    .await?;

                // Close the issue when the merger can't do it:
                // - Local issues: merger can't use `gh issue close`
                // - Multi-repo: merger runs in target repo, can't close god-repo issue
                let should_close =
                    issue.source == IssueOrigin::Local || issue.target_repo.is_some();

                if should_close {
                    let comment = issue.target_repo.as_ref().map_or_else(
                        || format!("Implemented in #{pr_number}"),
                        |repo_name| format!("Implemented in {repo_name}#{pr_number}"),
                    );
                    if let Err(e) = self.issues.close(issue.number, Some(&comment)).await {
                        warn!(
                            run_id = %run_id,
                            error = %e,
                            "failed to close issue"
                        );
                    }
                }

                (RunStatus::Complete, None)
            }
            Err(e) => {
                warn!(run_id = %run_id, error = %e, "pipeline failed");
                github::safe_comment(&self.github, pr_number, &format!("Pipeline failed: {e:#}"))
                    .await;
                let _ = self
                    .issues
                    .transition(
                        issue.number,
                        &self.config.labels.cooking,
                        &self.config.labels.failed,
                    )
                    .await;
                (RunStatus::Failed, Some(format!("{e:#}")))
            }
        };

        let conn = self.db.lock().await;
        db::runs::finish_run(&conn, run_id, final_status, error_msg.as_deref())
    }

    async fn run_steps(
        &self,
        run_id: &str,
        ctx: &AgentContext,
        worktree_path: &std::path::Path,
        auto_merge: bool,
    ) -> Result<()> {
        self.check_cancelled()?;

        // 1. Implement
        self.update_status(run_id, RunStatus::Implementing).await?;
        let impl_prompt = agents::implementer::build_prompt(ctx);
        self.run_agent(run_id, AgentRole::Implementer, &impl_prompt, worktree_path, 1).await?;

        git::push_branch(worktree_path, &ctx.branch).await?;

        // 2. Review-fix loop
        let clean = self.run_review_fix_loop(run_id, ctx, worktree_path).await?;

        if !clean {
            anyhow::bail!("unresolved findings after max review cycles");
        }

        // 3. Merge
        self.check_cancelled()?;
        ctx.pr_number.context("no PR number for merge step")?;
        self.update_status(run_id, RunStatus::Merging).await?;
        let merge_prompt = agents::merger::build_prompt(ctx, auto_merge);
        self.run_agent(run_id, AgentRole::Merger, &merge_prompt, worktree_path, 1).await?;

        Ok(())
    }

    async fn run_review_fix_loop(
        &self,
        run_id: &str,
        ctx: &AgentContext,
        worktree_path: &std::path::Path,
    ) -> Result<bool> {
        for cycle in 1..=2 {
            self.check_cancelled()?;

            self.update_status(run_id, RunStatus::Reviewing).await?;
            let review_prompt = agents::reviewer::build_prompt(ctx);
            let review_result = self
                .run_agent(run_id, AgentRole::Reviewer, &review_prompt, worktree_path, cycle)
                .await?;

            let review_output = parse_review_output(&review_result.output)?;
            self.store_findings(run_id, &review_output.findings).await?;

            let actionable: Vec<_> =
                review_output.findings.iter().filter(|f| f.severity != Severity::Info).collect();

            if actionable.is_empty() {
                info!(run_id = %run_id, cycle, "review clean");
                return Ok(true);
            }

            info!(run_id = %run_id, cycle, findings = actionable.len(), "review found issues");

            if cycle == 2 {
                let pr_number = ctx.pr_number.unwrap_or(0);
                let comment = format_unresolved_comment(&actionable);
                github::safe_comment(&self.github, pr_number, &comment).await;
                return Ok(false);
            }

            // Fix
            self.check_cancelled()?;
            self.update_status(run_id, RunStatus::Fixing).await?;

            let unresolved = {
                let conn = self.db.lock().await;
                db::agent_runs::get_unresolved_findings(&conn, run_id)?
            };

            let fix_prompt = agents::fixer::build_prompt(ctx, &unresolved);
            self.run_agent(run_id, AgentRole::Fixer, &fix_prompt, worktree_path, cycle).await?;

            git::push_branch(worktree_path, &ctx.branch).await?;
        }

        Ok(false)
    }

    async fn store_findings(&self, run_id: &str, findings: &[agents::Finding]) -> Result<()> {
        let agent_runs = {
            let conn = self.db.lock().await;
            db::agent_runs::get_agent_runs_for_run(&conn, run_id)?
        };
        let reviewer_run = agent_runs.iter().rev().find(|ar| ar.agent == "reviewer");
        if let Some(ar) = reviewer_run {
            let conn = self.db.lock().await;
            for finding in findings {
                let db_finding = ReviewFinding {
                    id: 0,
                    agent_run_id: ar.id,
                    severity: format!("{:?}", finding.severity).to_lowercase(),
                    category: finding.category.clone(),
                    file_path: finding.file_path.clone(),
                    line_number: finding.line_number,
                    message: finding.message.clone(),
                    resolved: false,
                };
                db::agent_runs::insert_finding(&conn, &db_finding)?;
            }
        }
        Ok(())
    }

    async fn run_agent(
        &self,
        run_id: &str,
        role: AgentRole,
        prompt: &str,
        working_dir: &std::path::Path,
        cycle: u32,
    ) -> Result<crate::process::AgentResult> {
        let agent_run_id = self.record_agent_start(run_id, role, cycle).await?;

        info!(run_id = %run_id, agent = %role, cycle, "agent starting");

        let invocation = AgentInvocation {
            role,
            prompt: prompt.to_string(),
            working_dir: working_dir.to_path_buf(),
        };

        let result = invoke_agent(self.runner.as_ref(), &invocation).await;

        match &result {
            Ok(agent_result) => {
                self.record_agent_success(run_id, agent_run_id, agent_result).await?;
            }
            Err(e) => {
                let conn = self.db.lock().await;
                db::agent_runs::finish_agent_run(
                    &conn,
                    agent_run_id,
                    "failed",
                    0.0,
                    0,
                    None,
                    Some(&format!("{e:#}")),
                )?;
            }
        }

        result
    }

    async fn record_agent_start(&self, run_id: &str, role: AgentRole, cycle: u32) -> Result<i64> {
        let agent_run = AgentRun {
            id: 0,
            run_id: run_id.to_string(),
            agent: role.to_string(),
            cycle,
            status: "running".to_string(),
            cost_usd: 0.0,
            turns: 0,
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: None,
            output_summary: None,
            error_message: None,
        };
        let conn = self.db.lock().await;
        db::agent_runs::insert_agent_run(&conn, &agent_run)
    }

    async fn record_agent_success(
        &self,
        run_id: &str,
        agent_run_id: i64,
        agent_result: &crate::process::AgentResult,
    ) -> Result<()> {
        let conn = self.db.lock().await;
        db::agent_runs::finish_agent_run(
            &conn,
            agent_run_id,
            "complete",
            agent_result.cost_usd,
            agent_result.turns,
            Some(&truncate(&agent_result.output, 500)),
            None,
        )?;

        let run = db::runs::get_run(&conn, run_id)?.context("run not found")?;
        let new_cost = run.cost_usd + agent_result.cost_usd;
        db::runs::update_run_cost(&conn, run_id, new_cost)?;
        drop(conn);

        if new_cost > self.config.pipeline.cost_budget {
            anyhow::bail!(
                "cost budget exceeded: ${:.2} > ${:.2}",
                new_cost,
                self.config.pipeline.cost_budget
            );
        }
        Ok(())
    }

    async fn update_status(&self, run_id: &str, status: RunStatus) -> Result<()> {
        let conn = self.db.lock().await;
        db::runs::update_run_status(&conn, run_id, status)
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancel_token.is_cancelled() {
            anyhow::bail!("pipeline cancelled");
        }
        Ok(())
    }
}

fn format_unresolved_comment(actionable: &[&agents::Finding]) -> String {
    let mut comment = String::from("## Unresolved findings after 2 review cycles\n\n");
    for f in actionable {
        let loc = match (&f.file_path, f.line_number) {
            (Some(path), Some(line)) => format!(" at `{path}:{line}`"),
            (Some(path), None) => format!(" in `{path}`"),
            _ => String::new(),
        };
        let _ = writeln!(comment, "- **[{:?}]** {}{}: {}", f.severity, f.category, loc, f.message);
    }
    comment
}

fn new_run(run_id: &str, issue: &PipelineIssue, auto_merge: bool) -> Run {
    let issue_source = match issue.source {
        IssueOrigin::Github => "github",
        IssueOrigin::Local => "local",
    };
    Run {
        id: run_id.to_string(),
        issue_number: issue.number,
        status: RunStatus::Pending,
        pr_number: None,
        branch: None,
        worktree_path: None,
        cost_usd: 0.0,
        auto_merge,
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        error_message: None,
        complexity: "full".to_string(),
        issue_source: issue_source.to_string(),
    }
}

/// Generate an 8-character hex run ID.
pub fn generate_run_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

/// Truncate a string to `max_len`, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len { s.to_string() } else { format!("{}...", &s[..max_len]) }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn run_ids_always_8_hex_chars(_seed in any::<u64>()) {
            let id = generate_run_id();
            prop_assert_eq!(id.len(), 8);
            prop_assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn run_id_is_8_hex_chars() {
        let id = generate_run_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn run_ids_are_unique() {
        let ids: Vec<_> = (0..100).map(|_| generate_run_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len());
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(100);
        let result = truncate(&long, 10);
        assert_eq!(result.len(), 13); // 10 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn format_unresolved_comment_includes_findings() {
        let findings = [
            agents::Finding {
                severity: Severity::Critical,
                category: "bug".to_string(),
                file_path: Some("src/main.rs".to_string()),
                line_number: Some(42),
                message: "null pointer".to_string(),
            },
            agents::Finding {
                severity: Severity::Warning,
                category: "style".to_string(),
                file_path: None,
                line_number: None,
                message: "missing docs".to_string(),
            },
        ];
        let refs: Vec<_> = findings.iter().collect();
        let comment = format_unresolved_comment(&refs);
        assert!(comment.contains("Unresolved findings"));
        assert!(comment.contains("null pointer"));
        assert!(comment.contains("`src/main.rs:42`"));
        assert!(comment.contains("missing docs"));
    }
}
