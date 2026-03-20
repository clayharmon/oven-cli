use std::{fmt::Write as _, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    agents::{
        self, AgentContext, AgentInvocation, AgentRole, Complexity, GraphContextNode,
        PlannerGraphOutput, Severity, invoke_agent, parse_fixer_output, parse_planner_graph_output,
        parse_review_output,
    },
    config::Config,
    db::{self, AgentRun, ReviewFinding, Run, RunStatus},
    git::{self, RebaseOutcome},
    github::{self, GhClient},
    issues::{IssueOrigin, IssueProvider, PipelineIssue},
    process::{self, CommandRunner},
};

/// The result of running an issue through the pipeline (before merge).
#[derive(Debug)]
pub struct PipelineOutcome {
    pub run_id: String,
    pub pr_number: u32,
    /// Worktree path, retained so the caller can clean up after merge.
    pub worktree_path: PathBuf,
    /// Repo directory the worktree belongs to (needed for `git::remove_worktree`).
    pub target_dir: PathBuf,
}

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
        let outcome = self.run_issue_pipeline(issue, auto_merge, complexity).await?;
        self.finalize_merge(&outcome, issue).await
    }

    /// Run the pipeline up to (but not including) finalization.
    ///
    /// Returns a `PipelineOutcome` with the run ID and PR number.
    /// The caller is responsible for calling `finalize_run` or `finalize_merge`
    /// at the appropriate time (e.g., after the PR is actually merged).
    pub async fn run_issue_pipeline(
        &self,
        issue: &PipelineIssue,
        auto_merge: bool,
        complexity: Option<Complexity>,
    ) -> Result<PipelineOutcome> {
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

        // Seed branch with an empty commit so GitHub accepts the draft PR
        git::empty_commit(
            &worktree.path,
            &format!("chore: start oven pipeline for issue #{}", issue.number),
        )
        .await?;

        info!(
            run_id = %run_id,
            issue = issue.number,
            branch = %worktree.branch,
            target_repo = ?issue.target_repo,
            "starting pipeline"
        );

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
            issue_source: issue.source.as_str().to_string(),
            base_branch: base_branch.clone(),
        };

        let result = self.run_steps(&run_id, &ctx, &worktree.path, auto_merge, &target_dir).await;

        if let Err(ref e) = result {
            // On failure, finalize immediately (no merge to wait for).
            // Worktree is intentionally preserved so uncommitted work is not lost.
            // Use `oven clean` to remove worktrees manually.
            self.finalize_run(&run_id, issue, pr_number, &result, &target_dir).await?;
            return Err(anyhow::anyhow!("{e:#}"));
        }

        // Update status to AwaitingMerge
        self.update_status(&run_id, RunStatus::AwaitingMerge).await?;

        Ok(PipelineOutcome { run_id, pr_number, worktree_path: worktree.path, target_dir })
    }

    /// Finalize a run after its PR has been merged.
    ///
    /// Transitions labels, closes issues, marks the run as complete, and cleans
    /// up the worktree that was left around while awaiting merge.
    pub async fn finalize_merge(
        &self,
        outcome: &PipelineOutcome,
        issue: &PipelineIssue,
    ) -> Result<()> {
        self.finalize_run(&outcome.run_id, issue, outcome.pr_number, &Ok(()), &outcome.target_dir)
            .await?;
        if let Err(e) = git::remove_worktree(&outcome.target_dir, &outcome.worktree_path).await {
            warn!(
                run_id = %outcome.run_id,
                error = %e,
                "failed to clean up worktree after merge"
            );
        }
        Ok(())
    }

    /// Invoke the planner agent to decide dependency ordering for a set of issues.
    ///
    /// `graph_context` describes the current dependency graph state so the planner
    /// can avoid scheduling conflicting work alongside in-flight issues.
    ///
    /// Returns `None` if the planner fails or returns unparseable output (fallback to default).
    pub async fn plan_issues(
        &self,
        issues: &[PipelineIssue],
        graph_context: &[GraphContextNode],
    ) -> Option<PlannerGraphOutput> {
        let prompt = match agents::planner::build_prompt(issues, graph_context) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "planner prompt build failed");
                return None;
            }
        };
        let invocation = AgentInvocation {
            role: AgentRole::Planner,
            prompt,
            working_dir: self.repo_dir.clone(),
            max_turns: Some(self.config.pipeline.turn_limit),
            model: self.config.models.model_for(AgentRole::Planner.as_str()).map(String::from),
        };

        match invoke_agent(self.runner.as_ref(), &invocation).await {
            Ok(result) => {
                debug!(output = %result.output, "raw planner output");
                let parsed = parse_planner_graph_output(&result.output);
                if parsed.is_none() {
                    warn!(output = %result.output, "planner returned unparseable output, falling back to all-parallel");
                }
                parsed
            }
            Err(e) => {
                warn!(error = %e, "planner agent failed, falling back to all-parallel");
                None
            }
        }
    }

    /// Determine the effective repo directory for worktrees and PRs.
    ///
    /// Returns `(target_dir, is_multi_repo)`. When multi-repo is disabled or no target
    /// is specified, falls back to `self.repo_dir`.
    pub(crate) fn resolve_target_dir(
        &self,
        target_repo: Option<&String>,
    ) -> Result<(PathBuf, bool)> {
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

    /// Reconstruct a `PipelineOutcome` from graph node data (for merge polling).
    ///
    /// Worktree paths are deterministic, so we can rebuild the outcome from
    /// the issue metadata stored on the graph node.
    pub fn reconstruct_outcome(
        &self,
        issue: &PipelineIssue,
        run_id: &str,
        pr_number: u32,
    ) -> Result<PipelineOutcome> {
        let (target_dir, _) = self.resolve_target_dir(issue.target_repo.as_ref())?;
        let worktree_path =
            target_dir.join(".oven").join("worktrees").join(format!("issue-{}", issue.number));
        Ok(PipelineOutcome { run_id: run_id.to_string(), pr_number, worktree_path, target_dir })
    }

    async fn record_worktree(&self, run_id: &str, worktree: &git::Worktree) -> Result<()> {
        let conn = self.db.lock().await;
        db::runs::update_run_worktree(
            &conn,
            run_id,
            &worktree.branch,
            &worktree.path.to_string_lossy(),
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
        target_dir: &std::path::Path,
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

                // Close the issue for local and multi-repo cases. Single-repo
                // GitHub issues are closed directly in the merge step (run_steps)
                // because they share the same gh context.
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
                github::safe_comment(
                    &self.github,
                    pr_number,
                    &format_pipeline_failure(e),
                    target_dir,
                )
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
        target_dir: &std::path::Path,
    ) -> Result<()> {
        self.check_cancelled()?;

        // 1. Implement
        self.update_status(run_id, RunStatus::Implementing).await?;
        let impl_prompt = agents::implementer::build_prompt(ctx)?;
        let impl_result =
            self.run_agent(run_id, AgentRole::Implementer, &impl_prompt, worktree_path, 1).await?;

        git::push_branch(worktree_path, &ctx.branch).await?;

        // 1b. Update PR description and mark ready for review
        if let Some(pr_number) = ctx.pr_number {
            let body = build_pr_body(&impl_result.output, ctx);
            if let Err(e) =
                self.github.edit_pr_in(pr_number, &pr_title(ctx), &body, target_dir).await
            {
                warn!(run_id = %run_id, error = %e, "failed to update PR description");
            }
            if let Err(e) = self.github.mark_pr_ready_in(pr_number, target_dir).await {
                warn!(run_id = %run_id, error = %e, "failed to mark PR ready");
            }
        }

        // 1c. Post implementation comment on PR
        if let Some(pr_number) = ctx.pr_number {
            let summary = extract_impl_summary(&impl_result.output);
            github::safe_comment(
                &self.github,
                pr_number,
                &format_impl_comment(&summary),
                target_dir,
            )
            .await;
        }

        // 2. Review-fix loop (posts its own step comments on the PR)
        self.run_review_fix_loop(run_id, ctx, worktree_path, target_dir).await?;

        // 3. Rebase onto base branch to resolve any conflicts from parallel merges
        self.check_cancelled()?;
        info!(run_id = %run_id, base = %ctx.base_branch, "rebasing onto base branch");
        let rebase_outcome =
            self.rebase_with_agent_fallback(run_id, ctx, worktree_path, target_dir).await?;

        if let Some(pr_number) = ctx.pr_number {
            github::safe_comment(
                &self.github,
                pr_number,
                &format_rebase_comment(&rebase_outcome),
                target_dir,
            )
            .await;
        }

        if let RebaseOutcome::Failed(ref msg) = rebase_outcome {
            anyhow::bail!("rebase failed: {msg}");
        }

        git::force_push_branch(worktree_path, &ctx.branch).await?;

        // 4. Merge (only when auto-merge is enabled)
        if auto_merge {
            self.check_cancelled()?;
            let pr_number = ctx.pr_number.context("no PR number for merge step")?;
            self.update_status(run_id, RunStatus::Merging).await?;

            self.github
                .merge_pr_in(pr_number, &self.config.pipeline.merge_strategy, target_dir)
                .await?;
            info!(run_id = %run_id, pr = pr_number, "PR merged");

            // Close the issue for single-repo GitHub issues. Multi-repo and local
            // issues are closed by finalize_run instead (different repo context).
            if ctx.target_repo.is_none() && ctx.issue_source == "github" {
                if let Err(e) = self
                    .github
                    .close_issue(ctx.issue_number, Some(&format!("Implemented in #{pr_number}")))
                    .await
                {
                    warn!(run_id = %run_id, error = %e, "failed to close issue after merge");
                }
            }

            github::safe_comment(&self.github, pr_number, &format_merge_comment(), target_dir)
                .await;
        } else if let Some(pr_number) = ctx.pr_number {
            github::safe_comment(&self.github, pr_number, &format_ready_comment(), target_dir)
                .await;
        }

        Ok(())
    }

    async fn run_review_fix_loop(
        &self,
        run_id: &str,
        ctx: &AgentContext,
        worktree_path: &std::path::Path,
        target_dir: &std::path::Path,
    ) -> Result<()> {
        let mut pre_fix_ref: Option<String> = None;

        for cycle in 1..=3 {
            self.check_cancelled()?;

            self.update_status(run_id, RunStatus::Reviewing).await?;

            let (prior_addressed, prior_disputes, prior_unresolved) =
                self.gather_prior_findings(run_id, cycle).await?;

            let review_prompt = agents::reviewer::build_prompt(
                ctx,
                &prior_addressed,
                &prior_disputes,
                &prior_unresolved,
                pre_fix_ref.as_deref(),
            )?;

            // Reviewer failure: skip review and continue (don't kill pipeline)
            let review_result = match self
                .run_agent(run_id, AgentRole::Reviewer, &review_prompt, worktree_path, cycle)
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    warn!(run_id = %run_id, cycle, error = %e, "reviewer agent failed, skipping review");
                    if let Some(pr_number) = ctx.pr_number {
                        github::safe_comment(
                            &self.github,
                            pr_number,
                            &format_review_skipped_comment(cycle, &e),
                            target_dir,
                        )
                        .await;
                    }
                    return Ok(());
                }
            };

            let review_output = parse_review_output(&review_result.output);
            self.store_findings(run_id, &review_output.findings).await?;

            let actionable: Vec<_> =
                review_output.findings.iter().filter(|f| f.severity != Severity::Info).collect();

            // Post review comment on PR
            if let Some(pr_number) = ctx.pr_number {
                github::safe_comment(
                    &self.github,
                    pr_number,
                    &format_review_comment(cycle, &actionable),
                    target_dir,
                )
                .await;
            }

            if actionable.is_empty() {
                info!(run_id = %run_id, cycle, "review clean");
                return Ok(());
            }

            info!(run_id = %run_id, cycle, findings = actionable.len(), "review found issues");

            if cycle == 3 {
                if let Some(pr_number) = ctx.pr_number {
                    let comment = format_unresolved_comment(&actionable);
                    github::safe_comment(&self.github, pr_number, &comment, target_dir).await;
                } else {
                    warn!(run_id = %run_id, "no PR number, cannot post unresolved findings");
                }
                return Ok(());
            }

            // Snapshot HEAD before fix step so next reviewer can scope to fixer changes
            pre_fix_ref = Some(git::head_sha(worktree_path).await?);

            self.run_fix_step(run_id, ctx, worktree_path, target_dir, cycle).await?;
        }

        Ok(())
    }

    /// Gather prior addressed, disputed, and unresolved findings for review cycles 2+.
    async fn gather_prior_findings(
        &self,
        run_id: &str,
        cycle: u32,
    ) -> Result<(Vec<ReviewFinding>, Vec<ReviewFinding>, Vec<ReviewFinding>)> {
        if cycle <= 1 {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }

        let conn = self.db.lock().await;
        let all_resolved = db::agent_runs::get_resolved_findings(&conn, run_id)?;
        let all_unresolved = db::agent_runs::get_unresolved_findings(&conn, run_id)?;
        drop(conn);

        let (mut addressed, disputed): (Vec<_>, Vec<_>) = all_resolved.into_iter().partition(|f| {
            f.dispute_reason.as_deref().is_some_and(|r| r.starts_with("ADDRESSED: "))
        });

        // Strip the "ADDRESSED: " prefix so the template gets clean action text
        for f in &mut addressed {
            if let Some(ref mut reason) = f.dispute_reason {
                if let Some(stripped) = reason.strip_prefix("ADDRESSED: ") {
                    *reason = stripped.to_string();
                }
            }
        }

        Ok((addressed, disputed, all_unresolved))
    }

    /// Run the fixer agent for a given cycle, process its output, and push.
    ///
    /// If the fixer agent fails, posts a comment on the PR and returns `Ok(())`
    /// so the caller can continue to the next review cycle.
    ///
    /// Handles three fixer outcome scenarios:
    /// 1. Normal: fixer produces structured JSON with addressed/disputed findings
    /// 2. Silent commits: fixer makes commits but no structured output (infer from git)
    /// 3. Did nothing: no commits and no output (mark findings as not actionable)
    async fn run_fix_step(
        &self,
        run_id: &str,
        ctx: &AgentContext,
        worktree_path: &std::path::Path,
        target_dir: &std::path::Path,
        cycle: u32,
    ) -> Result<()> {
        self.check_cancelled()?;
        self.update_status(run_id, RunStatus::Fixing).await?;

        let actionable = self.filter_actionable_findings(run_id).await?;

        if actionable.is_empty() {
            info!(run_id = %run_id, cycle, "no actionable findings for fixer, skipping");
            if let Some(pr_number) = ctx.pr_number {
                github::safe_comment(
                    &self.github,
                    pr_number,
                    &format!(
                        "### Fix skipped (cycle {cycle})\n\n\
                         No actionable findings (all findings lacked file paths).\
                         {COMMENT_FOOTER}"
                    ),
                    target_dir,
                )
                .await;
            }
            return Ok(());
        }

        // Snapshot HEAD before fixer runs
        let pre_fix_head = git::head_sha(worktree_path).await?;

        let fix_prompt = agents::fixer::build_prompt(ctx, &actionable)?;

        // Fixer failure: skip fix (caller continues to next review cycle)
        let fix_result =
            match self.run_agent(run_id, AgentRole::Fixer, &fix_prompt, worktree_path, cycle).await
            {
                Ok(result) => result,
                Err(e) => {
                    warn!(run_id = %run_id, cycle, error = %e, "fixer agent failed, skipping fix");
                    if let Some(pr_number) = ctx.pr_number {
                        github::safe_comment(
                            &self.github,
                            pr_number,
                            &format_fix_skipped_comment(cycle, &e),
                            target_dir,
                        )
                        .await;
                    }
                    return Ok(());
                }
            };

        // Parse fixer output and detect "did nothing" scenarios
        let fixer_output = parse_fixer_output(&fix_result.output);
        let fixer_did_nothing =
            fixer_output.addressed.is_empty() && fixer_output.disputed.is_empty();

        let new_commits = if fixer_did_nothing {
            git::commit_count_since(worktree_path, &pre_fix_head).await.unwrap_or(0)
        } else {
            0
        };

        if fixer_did_nothing {
            if new_commits > 0 {
                // Fixer made commits but didn't produce structured output.
                // Infer which findings were addressed by checking changed files.
                warn!(
                    run_id = %run_id, cycle, commits = new_commits,
                    "fixer output unparseable but commits exist, inferring addressed from git"
                );
                self.infer_addressed_from_git(run_id, &actionable, worktree_path, &pre_fix_head)
                    .await?;
            } else {
                // Fixer did literally nothing. Mark findings so they don't zombie.
                warn!(
                    run_id = %run_id, cycle,
                    "fixer produced no output and no commits, marking findings not actionable"
                );
                let conn = self.db.lock().await;
                for f in &actionable {
                    db::agent_runs::resolve_finding(
                        &conn,
                        f.id,
                        "ADDRESSED: fixer could not act on this finding (no commits, no output)",
                    )?;
                }
                drop(conn);
            }
        } else {
            self.process_fixer_results(run_id, &actionable, &fixer_output).await?;
        }

        // Post fix comment on PR
        if let Some(pr_number) = ctx.pr_number {
            let comment = if fixer_did_nothing {
                format_fixer_recovery_comment(cycle, new_commits)
            } else {
                format_fix_comment(cycle, &fixer_output)
            };
            github::safe_comment(&self.github, pr_number, &comment, target_dir).await;
        }

        git::push_branch(worktree_path, &ctx.branch).await?;
        Ok(())
    }

    /// Process all fixer results (disputes + addressed) in a single lock acquisition.
    ///
    /// The fixer references findings by 1-indexed position in the list it received.
    /// We map those back to the actual `ReviewFinding` IDs and mark them resolved.
    /// Disputed findings store the fixer's reason directly; addressed findings get
    /// an `ADDRESSED: ` prefix so we can distinguish them when building the next
    /// reviewer prompt.
    async fn process_fixer_results(
        &self,
        run_id: &str,
        findings_sent_to_fixer: &[ReviewFinding],
        fixer_output: &agents::FixerOutput,
    ) -> Result<()> {
        if fixer_output.disputed.is_empty() && fixer_output.addressed.is_empty() {
            return Ok(());
        }

        let conn = self.db.lock().await;

        for dispute in &fixer_output.disputed {
            let idx = dispute.finding.saturating_sub(1) as usize;
            if let Some(finding) = findings_sent_to_fixer.get(idx) {
                db::agent_runs::resolve_finding(&conn, finding.id, &dispute.reason)?;
                info!(
                    run_id = %run_id,
                    finding_id = finding.id,
                    reason = %dispute.reason,
                    "finding disputed by fixer, marked resolved"
                );
            }
        }

        for action in &fixer_output.addressed {
            let idx = action.finding.saturating_sub(1) as usize;
            if let Some(finding) = findings_sent_to_fixer.get(idx) {
                let reason = format!("ADDRESSED: {}", action.action);
                db::agent_runs::resolve_finding(&conn, finding.id, &reason)?;
                info!(
                    run_id = %run_id,
                    finding_id = finding.id,
                    action = %action.action,
                    "finding addressed by fixer, marked resolved"
                );
            }
        }

        drop(conn);
        Ok(())
    }

    /// Filter unresolved findings into actionable (has `file_path`) and non-actionable.
    ///
    /// Non-actionable findings are auto-resolved in the DB so they don't accumulate
    /// as zombie findings across cycles.
    async fn filter_actionable_findings(&self, run_id: &str) -> Result<Vec<ReviewFinding>> {
        let conn = self.db.lock().await;
        let unresolved = db::agent_runs::get_unresolved_findings(&conn, run_id)?;

        let (actionable, non_actionable): (Vec<_>, Vec<_>) =
            unresolved.into_iter().partition(|f| f.file_path.is_some());

        if !non_actionable.is_empty() {
            warn!(
                run_id = %run_id,
                count = non_actionable.len(),
                "auto-resolving non-actionable findings (no file_path)"
            );
            for f in &non_actionable {
                db::agent_runs::resolve_finding(
                    &conn,
                    f.id,
                    "ADDRESSED: auto-resolved -- finding has no file path, not actionable by fixer",
                )?;
            }
        }

        drop(conn);
        Ok(actionable)
    }

    /// Infer which findings were addressed by the fixer based on git changes.
    ///
    /// When the fixer makes commits but doesn't produce structured JSON output,
    /// we cross-reference the changed files against the finding file paths.
    async fn infer_addressed_from_git(
        &self,
        run_id: &str,
        findings: &[ReviewFinding],
        worktree_path: &std::path::Path,
        pre_fix_head: &str,
    ) -> Result<()> {
        let changed_files =
            git::changed_files_since(worktree_path, pre_fix_head).await.unwrap_or_default();

        let conn = self.db.lock().await;
        for f in findings {
            let was_touched =
                f.file_path.as_ref().is_some_and(|fp| changed_files.iter().any(|cf| cf == fp));

            let reason = if was_touched {
                "ADDRESSED: inferred from git -- fixer modified this file (no structured output)"
            } else {
                "ADDRESSED: inferred from git -- file not modified (no structured output)"
            };

            db::agent_runs::resolve_finding(&conn, f.id, reason)?;
            info!(
                run_id = %run_id,
                finding_id = f.id,
                file = ?f.file_path,
                touched = was_touched,
                "finding resolved via git inference"
            );
        }
        drop(conn);
        Ok(())
    }

    async fn store_findings(&self, run_id: &str, findings: &[agents::Finding]) -> Result<()> {
        let conn = self.db.lock().await;
        let agent_runs = db::agent_runs::get_agent_runs_for_run(&conn, run_id)?;
        let reviewer_run_id = agent_runs
            .iter()
            .rev()
            .find_map(|ar| if ar.agent == "reviewer" { Some(ar.id) } else { None });
        if let Some(ar_id) = reviewer_run_id {
            for finding in findings {
                let db_finding = ReviewFinding {
                    id: 0,
                    agent_run_id: ar_id,
                    severity: finding.severity.to_string(),
                    category: finding.category.clone(),
                    file_path: finding.file_path.clone(),
                    line_number: finding.line_number,
                    message: finding.message.clone(),
                    resolved: false,
                    dispute_reason: None,
                };
                db::agent_runs::insert_finding(&conn, &db_finding)?;
            }
        }
        drop(conn);
        Ok(())
    }

    /// Rebase with agent-assisted conflict resolution.
    ///
    /// Chain: rebase -> if conflicts, agent resolves -> rebase --continue -> loop.
    async fn rebase_with_agent_fallback(
        &self,
        run_id: &str,
        ctx: &AgentContext,
        worktree_path: &std::path::Path,
        target_dir: &std::path::Path,
    ) -> Result<RebaseOutcome> {
        const MAX_REBASE_ROUNDS: u32 = 5;

        let outcome = git::start_rebase(worktree_path, &ctx.base_branch).await;

        let mut conflicting_files = match outcome {
            RebaseOutcome::RebaseConflicts(files) => files,
            other => return Ok(other),
        };

        for round in 1..=MAX_REBASE_ROUNDS {
            self.check_cancelled()?;
            info!(
                run_id = %run_id,
                round,
                files = ?conflicting_files,
                "rebase conflicts, attempting agent resolution"
            );

            if let Some(pr_number) = ctx.pr_number {
                github::safe_comment(
                    &self.github,
                    pr_number,
                    &format_rebase_conflict_comment(round, &conflicting_files),
                    target_dir,
                )
                .await;
            }

            let conflict_prompt = format!(
                "You are resolving rebase conflicts. The following files have unresolved \
                 conflict markers (<<<<<<< / ======= / >>>>>>> markers):\n\n{}\n\n\
                 Open each file, find the conflict markers, and resolve them by choosing \
                 the correct code. Remove all conflict markers. Do NOT add new features \
                 or refactor -- just resolve the conflicts so the code compiles and tests pass.\n\n\
                 After resolving, run any test/lint commands if available:\n\
                 - Test: {}\n\
                 - Lint: {}",
                conflicting_files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"),
                ctx.test_command.as_deref().unwrap_or("(none)"),
                ctx.lint_command.as_deref().unwrap_or("(none)"),
            );

            if let Err(e) = self
                .run_agent(run_id, AgentRole::Implementer, &conflict_prompt, worktree_path, 1)
                .await
            {
                warn!(run_id = %run_id, error = %e, "conflict resolution agent failed");
                git::abort_rebase(worktree_path).await;
                return Ok(RebaseOutcome::Failed(format!(
                    "agent conflict resolution failed: {e:#}"
                )));
            }

            // Check if the agent actually resolved the conflicts
            let remaining = git::conflicting_files(worktree_path).await;
            if !remaining.is_empty() {
                warn!(
                    run_id = %run_id,
                    remaining = ?remaining,
                    "agent did not resolve all conflicts"
                );
                git::abort_rebase(worktree_path).await;
                return Ok(RebaseOutcome::Failed(format!(
                    "agent could not resolve conflicts in: {}",
                    remaining.join(", ")
                )));
            }

            // Stage resolved files and continue the rebase
            match git::rebase_continue(worktree_path, &conflicting_files).await {
                Ok(None) => {
                    info!(run_id = %run_id, "agent resolved rebase conflicts");
                    return Ok(RebaseOutcome::AgentResolved);
                }
                Ok(Some(new_conflicts)) => {
                    // Next commit in the rebase also has conflicts -- loop
                    conflicting_files = new_conflicts;
                }
                Err(e) => {
                    git::abort_rebase(worktree_path).await;
                    return Ok(RebaseOutcome::Failed(format!("rebase --continue failed: {e:#}")));
                }
            }
        }

        // Exhausted all rounds
        git::abort_rebase(worktree_path).await;
        Ok(RebaseOutcome::Failed(format!(
            "rebase conflicts persisted after {MAX_REBASE_ROUNDS} resolution rounds"
        )))
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
            max_turns: Some(self.config.pipeline.turn_limit),
            model: self.config.models.model_for(role.as_str()).map(String::from),
        };

        let result = process::run_with_retry(self.runner.as_ref(), &invocation).await;

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
                    None,
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
            raw_output: None,
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
            Some(&agent_result.output),
        )?;

        let new_cost = db::runs::increment_run_cost(&conn, run_id, agent_result.cost_usd)?;
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

const COMMENT_FOOTER: &str =
    "\n---\nAutomated by [oven](https://github.com/clayharmon/oven-cli) \u{1F35E}";

fn format_unresolved_comment(actionable: &[&agents::Finding]) -> String {
    let mut comment = String::from(
        "### Unresolved review findings\n\n\
         The review-fix loop ran 2 cycles but these findings remain unresolved.\n",
    );

    // Group findings by severity
    for severity in &[Severity::Critical, Severity::Warning] {
        let group: Vec<_> = actionable.iter().filter(|f| &f.severity == severity).collect();
        if group.is_empty() {
            continue;
        }
        let heading = match severity {
            Severity::Critical => "Critical",
            Severity::Warning => "Warning",
            Severity::Info => unreachable!("loop only iterates Critical and Warning"),
        };
        let _ = writeln!(comment, "\n#### {heading}\n");
        for f in group {
            let loc = match (&f.file_path, f.line_number) {
                (Some(path), Some(line)) => format!(" in `{path}:{line}`"),
                (Some(path), None) => format!(" in `{path}`"),
                _ => String::new(),
            };
            let _ = writeln!(comment, "- **{}**{} -- {}", f.category, loc, f.message);
        }
    }

    comment.push_str(COMMENT_FOOTER);
    comment
}

fn format_impl_comment(summary: &str) -> String {
    format!("### Implementation complete\n\n{summary}{COMMENT_FOOTER}")
}

fn format_review_comment(cycle: u32, actionable: &[&agents::Finding]) -> String {
    if actionable.is_empty() {
        return format!(
            "### Review complete (cycle {cycle})\n\n\
             Clean review, no actionable findings.{COMMENT_FOOTER}"
        );
    }

    let mut comment = format!(
        "### Review complete (cycle {cycle})\n\n\
         **{count} finding{s}:**\n",
        count = actionable.len(),
        s = if actionable.len() == 1 { "" } else { "s" },
    );

    for f in actionable {
        let loc = match (&f.file_path, f.line_number) {
            (Some(path), Some(line)) => format!(" in `{path}:{line}`"),
            (Some(path), None) => format!(" in `{path}`"),
            _ => String::new(),
        };
        let _ = writeln!(
            comment,
            "- [{sev}] **{cat}**{loc} -- {msg}",
            sev = f.severity,
            cat = f.category,
            msg = f.message,
        );
    }

    comment.push_str(COMMENT_FOOTER);
    comment
}

fn format_fix_comment(cycle: u32, fixer: &agents::FixerOutput) -> String {
    let addressed = fixer.addressed.len();
    let disputed = fixer.disputed.len();
    format!(
        "### Fix complete (cycle {cycle})\n\n\
         **Addressed:** {addressed} finding{a_s}\n\
         **Disputed:** {disputed} finding{d_s}{COMMENT_FOOTER}",
        a_s = if addressed == 1 { "" } else { "s" },
        d_s = if disputed == 1 { "" } else { "s" },
    )
}

fn format_rebase_conflict_comment(round: u32, conflicting_files: &[String]) -> String {
    format!(
        "### Resolving rebase conflicts (round {round})\n\n\
         Attempting agent-assisted resolution for {} conflicting file{}: \
         {}{COMMENT_FOOTER}",
        conflicting_files.len(),
        if conflicting_files.len() == 1 { "" } else { "s" },
        conflicting_files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", "),
    )
}

fn format_fixer_recovery_comment(cycle: u32, new_commits: u32) -> String {
    if new_commits > 0 {
        format!(
            "### Fix complete (cycle {cycle})\n\n\
             Fixer made {new_commits} commit{s} but did not produce structured output. \
             Addressed findings inferred from changed files.{COMMENT_FOOTER}",
            s = if new_commits == 1 { "" } else { "s" },
        )
    } else {
        format!(
            "### Fix complete (cycle {cycle})\n\n\
             Fixer could not act on the findings (no code changes made). \
             Findings marked as not actionable.{COMMENT_FOOTER}"
        )
    }
}

fn format_review_skipped_comment(cycle: u32, error: &anyhow::Error) -> String {
    format!(
        "### Review skipped (cycle {cycle})\n\n\
         Reviewer agent encountered an error. Continuing without review.\n\n\
         **Error:** {error:#}{COMMENT_FOOTER}"
    )
}

fn format_fix_skipped_comment(cycle: u32, error: &anyhow::Error) -> String {
    format!(
        "### Fix skipped (cycle {cycle})\n\n\
         Fixer agent encountered an error. Continuing to next cycle.\n\n\
         **Error:** {error:#}{COMMENT_FOOTER}"
    )
}

fn format_rebase_comment(outcome: &RebaseOutcome) -> String {
    match outcome {
        RebaseOutcome::Clean => {
            format!("### Rebase\n\nRebased onto base branch cleanly.{COMMENT_FOOTER}")
        }
        RebaseOutcome::AgentResolved => {
            format!(
                "### Rebase\n\n\
                 Rebase had conflicts. Agent resolved them.{COMMENT_FOOTER}"
            )
        }
        RebaseOutcome::RebaseConflicts(_) => {
            format!(
                "### Rebase\n\n\
                 Rebase conflicts present (awaiting resolution).{COMMENT_FOOTER}"
            )
        }
        RebaseOutcome::Failed(msg) => {
            format!(
                "### Rebase failed\n\n\
                 Could not rebase onto the base branch.\n\n\
                 **Error:** {msg}{COMMENT_FOOTER}"
            )
        }
    }
}

fn format_ready_comment() -> String {
    format!(
        "### Ready for review\n\nPipeline complete. This PR is ready for manual review.{COMMENT_FOOTER}"
    )
}

fn format_merge_comment() -> String {
    format!("### Merged\n\nPipeline complete. PR has been merged.{COMMENT_FOOTER}")
}

fn format_pipeline_failure(e: &anyhow::Error) -> String {
    format!(
        "## Pipeline failed\n\n\
         **Error:** {e:#}\n\n\
         The pipeline hit an unrecoverable error. Check the run logs for detail, \
         or re-run the pipeline.\
         {COMMENT_FOOTER}"
    )
}

/// Build a PR title using the issue metadata.
///
/// Infers a conventional-commit prefix from the issue title. Falls back to
/// `fix` when no keyword matches.
fn pr_title(ctx: &AgentContext) -> String {
    let prefix = infer_commit_type(&ctx.issue_title);
    if ctx.issue_source == "github" {
        format!("{prefix}(#{}): {}", ctx.issue_number, ctx.issue_title)
    } else {
        format!("{prefix}: {}", ctx.issue_title)
    }
}

/// Infer a conventional-commit type from an issue title.
fn infer_commit_type(title: &str) -> &'static str {
    let lower = title.to_lowercase();
    if lower.starts_with("feat") || lower.contains("add ") || lower.contains("implement ") {
        "feat"
    } else if lower.starts_with("refactor") {
        "refactor"
    } else if lower.starts_with("docs") || lower.starts_with("document") {
        "docs"
    } else if lower.starts_with("test") || lower.starts_with("add test") {
        "test"
    } else if lower.starts_with("chore") {
        "chore"
    } else {
        "fix"
    }
}

/// Build a full PR body from the implementer's output and issue context.
fn build_pr_body(impl_output: &str, ctx: &AgentContext) -> String {
    let issue_ref = if ctx.issue_source == "github" {
        format!("Resolves #{}", ctx.issue_number)
    } else {
        format!("From local issue #{}", ctx.issue_number)
    };

    let summary = extract_impl_summary(impl_output);

    let mut body = String::new();
    let _ = writeln!(body, "{issue_ref}\n");
    let _ = write!(body, "{summary}");
    body.push_str(COMMENT_FOOTER);
    body
}

/// Extract the summary section from implementer output.
///
/// Looks for `## PR Template` (repo-specific PR template) or `## Changes Made`
/// (default format) headings. Falls back to the full output (truncated) if
/// neither heading is found.
fn extract_impl_summary(output: &str) -> String {
    // Prefer a filled-out PR template if the implementer found one
    let idx = output.find("## PR Template").or_else(|| output.find("## Changes Made"));

    if let Some(idx) = idx {
        let summary = output[idx..].trim();
        // Strip the "## PR Template" heading itself so the body reads cleanly
        let summary = summary.strip_prefix("## PR Template").map_or(summary, |s| s.trim_start());
        if summary.len() <= 4000 {
            return summary.to_string();
        }
        return truncate(summary, 4000);
    }
    // Fallback: no structured summary found. Don't dump raw agent narration
    // (stream-of-consciousness "Let me read..." text) into the PR body.
    String::from("*No implementation summary available. See commit history for details.*")
}

fn new_run(run_id: &str, issue: &PipelineIssue, auto_merge: bool) -> Run {
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
        issue_source: issue.source.to_string(),
    }
}

/// Generate an 8-character hex run ID.
pub fn generate_run_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

/// Truncate a string to at most `max_len` bytes, appending "..." if truncated.
///
/// Reserves 3 bytes for the "..." suffix so the total output never exceeds `max_len`.
/// Always cuts at a valid UTF-8 character boundary to avoid panics on multi-byte input.
pub(crate) fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let target = max_len.saturating_sub(3);
    let mut end = target;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
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
        assert_eq!(result.len(), 10); // 7 chars + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_multibyte_does_not_panic() {
        // Each emoji is 4 bytes. "😀😀😀" = 12 bytes.
        // max_len=8, target=5, walks back to boundary at 4 (one emoji).
        let s = "😀😀😀";
        let result = truncate(s, 8);
        assert!(result.ends_with("..."));
        assert!(result.starts_with("😀"));
        assert!(result.len() <= 8);
    }

    #[test]
    fn truncate_cjk_boundary() {
        // CJK chars are 3 bytes each
        let s = "你好世界测试"; // 18 bytes
        // max_len=10, target=7, walks back to boundary at 6 (two 3-byte chars).
        let result = truncate(s, 10);
        assert!(result.ends_with("..."));
        assert!(result.starts_with("你好"));
        assert!(result.len() <= 10);
    }

    #[test]
    fn extract_impl_summary_finds_changes_made() {
        let output = "Some preamble text\n\n## Changes Made\n- src/foo.rs: added bar\n\n## Tests Added\n- tests/foo.rs: bar test\n";
        let summary = extract_impl_summary(output);
        assert!(summary.starts_with("## Changes Made"));
        assert!(summary.contains("added bar"));
        assert!(summary.contains("## Tests Added"));
    }

    #[test]
    fn extract_impl_summary_prefers_pr_template() {
        let output = "Preamble\n\n## PR Template\n## Summary\n- Added auth flow\n\n## Testing\n- Unit tests pass\n";
        let summary = extract_impl_summary(output);
        // Should strip the "## PR Template" heading
        assert!(!summary.contains("## PR Template"));
        assert!(summary.starts_with("## Summary"));
        assert!(summary.contains("Added auth flow"));
    }

    #[test]
    fn extract_impl_summary_fallback_on_no_heading() {
        let output = "just some raw agent output with no structure";
        let summary = extract_impl_summary(output);
        assert_eq!(
            summary,
            "*No implementation summary available. See commit history for details.*"
        );
    }

    #[test]
    fn extract_impl_summary_empty_output() {
        let placeholder = "*No implementation summary available. See commit history for details.*";
        assert_eq!(extract_impl_summary(""), placeholder);
        assert_eq!(extract_impl_summary("   "), placeholder);
    }

    #[test]
    fn build_pr_body_github_issue() {
        let ctx = AgentContext {
            issue_number: 42,
            issue_title: "fix the thing".to_string(),
            issue_body: String::new(),
            branch: "oven/issue-42".to_string(),
            pr_number: Some(10),
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
            issue_source: "github".to_string(),
            base_branch: "main".to_string(),
        };
        let body = build_pr_body("## Changes Made\n- added stuff", &ctx);
        assert!(body.contains("Resolves #42"));
        assert!(body.contains("## Changes Made"));
        assert!(body.contains("Automated by [oven]"));
    }

    #[test]
    fn build_pr_body_local_issue() {
        let ctx = AgentContext {
            issue_number: 7,
            issue_title: "local thing".to_string(),
            issue_body: String::new(),
            branch: "oven/issue-7".to_string(),
            pr_number: Some(10),
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
            issue_source: "local".to_string(),
            base_branch: "main".to_string(),
        };
        let body = build_pr_body("## Changes Made\n- did local stuff", &ctx);
        assert!(body.contains("From local issue #7"));
        assert!(body.contains("## Changes Made"));
    }

    #[test]
    fn pr_title_github() {
        let ctx = AgentContext {
            issue_number: 42,
            issue_title: "fix the thing".to_string(),
            issue_body: String::new(),
            branch: String::new(),
            pr_number: None,
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
            issue_source: "github".to_string(),
            base_branch: "main".to_string(),
        };
        assert_eq!(pr_title(&ctx), "fix(#42): fix the thing");
    }

    #[test]
    fn pr_title_local() {
        let ctx = AgentContext {
            issue_number: 7,
            issue_title: "local thing".to_string(),
            issue_body: String::new(),
            branch: String::new(),
            pr_number: None,
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
            issue_source: "local".to_string(),
            base_branch: "main".to_string(),
        };
        assert_eq!(pr_title(&ctx), "fix: local thing");
    }

    #[test]
    fn infer_commit_type_feat() {
        assert_eq!(infer_commit_type("Add dark mode support"), "feat");
        assert_eq!(infer_commit_type("Implement caching layer"), "feat");
        assert_eq!(infer_commit_type("Feature: new dashboard"), "feat");
    }

    #[test]
    fn infer_commit_type_refactor() {
        assert_eq!(infer_commit_type("Refactor auth middleware"), "refactor");
    }

    #[test]
    fn infer_commit_type_docs() {
        assert_eq!(infer_commit_type("Document the API endpoints"), "docs");
        assert_eq!(infer_commit_type("Docs: update README"), "docs");
    }

    #[test]
    fn infer_commit_type_defaults_to_fix() {
        assert_eq!(infer_commit_type("Null pointer in config parser"), "fix");
        assert_eq!(infer_commit_type("Crash on empty input"), "fix");
    }

    #[test]
    fn pr_title_feat_github() {
        let ctx = AgentContext {
            issue_number: 10,
            issue_title: "Add dark mode".to_string(),
            issue_body: String::new(),
            branch: String::new(),
            pr_number: None,
            test_command: None,
            lint_command: None,
            review_findings: None,
            cycle: 1,
            target_repo: None,
            issue_source: "github".to_string(),
            base_branch: "main".to_string(),
        };
        assert_eq!(pr_title(&ctx), "feat(#10): Add dark mode");
    }

    #[test]
    fn format_unresolved_comment_groups_by_severity() {
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
        assert!(comment.contains("#### Critical"));
        assert!(comment.contains("#### Warning"));
        assert!(comment.contains("**bug** in `src/main.rs:42` -- null pointer"));
        assert!(comment.contains("**style** -- missing docs"));
        assert!(comment.contains("Automated by [oven]"));
    }

    #[test]
    fn format_unresolved_comment_skips_empty_severity_groups() {
        let findings = [agents::Finding {
            severity: Severity::Warning,
            category: "testing".to_string(),
            file_path: Some("src/lib.rs".to_string()),
            line_number: None,
            message: "missing edge case test".to_string(),
        }];
        let refs: Vec<_> = findings.iter().collect();
        let comment = format_unresolved_comment(&refs);
        assert!(!comment.contains("#### Critical"));
        assert!(comment.contains("#### Warning"));
    }

    #[test]
    fn format_pipeline_failure_includes_error() {
        let err = anyhow::anyhow!("cost budget exceeded: $12.50 > $10.00");
        let comment = format_pipeline_failure(&err);
        assert!(comment.contains("## Pipeline failed"));
        assert!(comment.contains("cost budget exceeded"));
        assert!(comment.contains("Automated by [oven]"));
    }

    #[test]
    fn format_impl_comment_includes_summary() {
        let comment = format_impl_comment("Added login endpoint with tests");
        assert!(comment.contains("### Implementation complete"));
        assert!(comment.contains("Added login endpoint with tests"));
        assert!(comment.contains("Automated by [oven]"));
    }

    #[test]
    fn format_review_comment_clean() {
        let comment = format_review_comment(1, &[]);
        assert!(comment.contains("### Review complete (cycle 1)"));
        assert!(comment.contains("Clean review"));
    }

    #[test]
    fn format_review_comment_with_findings() {
        let findings = [agents::Finding {
            severity: Severity::Critical,
            category: "bug".to_string(),
            file_path: Some("src/main.rs".to_string()),
            line_number: Some(42),
            message: "null pointer".to_string(),
        }];
        let refs: Vec<_> = findings.iter().collect();
        let comment = format_review_comment(1, &refs);
        assert!(comment.contains("### Review complete (cycle 1)"));
        assert!(comment.contains("1 finding"));
        assert!(comment.contains("[critical]"));
        assert!(comment.contains("`src/main.rs:42`"));
    }

    #[test]
    fn format_fix_comment_counts() {
        let fixer = agents::FixerOutput {
            addressed: vec![
                agents::FixerAction { finding: 1, action: "fixed it".to_string() },
                agents::FixerAction { finding: 2, action: "also fixed".to_string() },
            ],
            disputed: vec![agents::FixerDispute { finding: 3, reason: "not a bug".to_string() }],
        };
        let comment = format_fix_comment(1, &fixer);
        assert!(comment.contains("### Fix complete (cycle 1)"));
        assert!(comment.contains("Addressed:** 2 findings"));
        assert!(comment.contains("Disputed:** 1 finding\n"));
    }

    #[test]
    fn format_rebase_comment_variants() {
        let clean = format_rebase_comment(&RebaseOutcome::Clean);
        assert!(clean.contains("Rebased onto base branch cleanly"));

        let agent = format_rebase_comment(&RebaseOutcome::AgentResolved);
        assert!(agent.contains("Agent resolved them"));

        let conflicts =
            format_rebase_comment(&RebaseOutcome::RebaseConflicts(vec!["foo.rs".into()]));
        assert!(conflicts.contains("awaiting resolution"));

        let failed = format_rebase_comment(&RebaseOutcome::Failed("conflict in foo.rs".into()));
        assert!(failed.contains("Rebase failed"));
        assert!(failed.contains("conflict in foo.rs"));
    }

    #[test]
    fn format_ready_comment_content() {
        let comment = format_ready_comment();
        assert!(comment.contains("### Ready for review"));
        assert!(comment.contains("manual review"));
    }

    #[test]
    fn format_merge_comment_content() {
        let comment = format_merge_comment();
        assert!(comment.contains("### Merged"));
    }
}
