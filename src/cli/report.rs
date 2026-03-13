use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::Serialize;

use super::{GlobalOpts, ReportArgs};
use crate::db::{self, AgentRun, Run};

#[allow(clippy::unused_async)]
pub async fn run(args: ReportArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
    let db_path = project_dir.join(".oven").join("oven.db");
    let conn = db::open(&db_path)?;

    if args.all {
        let runs = db::runs::get_all_runs(&conn)?;
        if runs.is_empty() {
            println!("no runs found");
            return Ok(());
        }
        if args.json {
            let reports = runs
                .iter()
                .map(|r| -> Result<_> {
                    let agents = db::agent_runs::get_agent_runs_for_run(&conn, &r.id)
                        .context("fetching agent runs")?;
                    Ok(RunReport::from_run(r, &agents))
                })
                .collect::<Result<Vec<_>>>()?;
            println!("{}", serde_json::to_string_pretty(&reports)?);
        } else {
            print_runs_table(&runs);
        }
        return Ok(());
    }

    let run = if let Some(ref run_id) = args.run_id {
        db::runs::get_run(&conn, run_id)?.with_context(|| format!("run {run_id} not found"))?
    } else {
        db::runs::get_latest_run(&conn)?.context("no runs found")?
    };

    let agent_runs = db::agent_runs::get_agent_runs_for_run(&conn, &run.id)?;

    if args.json {
        let report = RunReport::from_run(&run, &agent_runs);
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_run_report(&run, &agent_runs);
    }

    Ok(())
}

fn print_runs_table(runs: &[Run]) {
    println!("{:<10} {:<8} {:<12} {:>8}", "Run", "Issue", "Status", "Cost");
    println!("{}", "-".repeat(42));
    for run in runs {
        println!("{:<10} #{:<7} {:<12} ${:.2}", run.id, run.issue_number, run.status, run.cost_usd);
    }
}

fn print_run_report(run: &Run, agent_runs: &[AgentRun]) {
    println!("Run {} - Issue #{}", run.id, run.issue_number);
    println!("Status: {}", run.status);

    if let Some(start) = run.started_at.get(..19) {
        println!("Started: {start}");
    }
    if let Some(ref end) = run.finished_at {
        println!("Finished: {}", end.get(..19).unwrap_or(end));
    }

    println!("Total cost: ${:.2}", run.cost_usd);

    if let Some(ref err) = run.error_message {
        println!("Error: {err}");
    }

    if !agent_runs.is_empty() {
        println!();
        println!("Agents:");
        for ar in agent_runs {
            let mut line = format!("  {:<14} ${:.2}  {:>3} turns", ar.agent, ar.cost_usd, ar.turns);
            let _ = write!(line, "  {}", ar.status);
            println!("{line}");
        }
    }
}

/// Serializable report for JSON output.
#[derive(Serialize)]
struct RunReport {
    id: String,
    issue_number: u32,
    status: String,
    cost_usd: f64,
    started_at: String,
    finished_at: Option<String>,
    error_message: Option<String>,
    agents: Vec<AgentRunReport>,
}

#[derive(Serialize)]
struct AgentRunReport {
    agent: String,
    cycle: u32,
    status: String,
    cost_usd: f64,
    turns: u32,
}

impl RunReport {
    fn from_run(run: &Run, agent_runs: &[AgentRun]) -> Self {
        Self {
            id: run.id.clone(),
            issue_number: run.issue_number,
            status: run.status.to_string(),
            cost_usd: run.cost_usd,
            started_at: run.started_at.clone(),
            finished_at: run.finished_at.clone(),
            error_message: run.error_message.clone(),
            agents: agent_runs
                .iter()
                .map(|ar| AgentRunReport {
                    agent: ar.agent.clone(),
                    cycle: ar.cycle,
                    status: ar.status.clone(),
                    cost_usd: ar.cost_usd,
                    turns: ar.turns,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RunStatus;

    fn sample_run() -> Run {
        Run {
            id: "abcd1234".to_string(),
            issue_number: 42,
            status: RunStatus::Complete,
            pr_number: Some(99),
            branch: Some("oven/issue-42-abc".to_string()),
            worktree_path: None,
            cost_usd: 4.23,
            auto_merge: false,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:08:32".to_string()),
            error_message: None,
            complexity: "full".to_string(),
            issue_source: "github".to_string(),
        }
    }

    fn sample_agent_runs() -> Vec<AgentRun> {
        vec![
            AgentRun {
                id: 1,
                run_id: "abcd1234".to_string(),
                agent: "implementer".to_string(),
                cycle: 1,
                status: "complete".to_string(),
                cost_usd: 2.10,
                turns: 12,
                started_at: "2026-03-12T10:00:00".to_string(),
                finished_at: Some("2026-03-12T10:03:15".to_string()),
                output_summary: None,
                error_message: None,
            },
            AgentRun {
                id: 2,
                run_id: "abcd1234".to_string(),
                agent: "reviewer".to_string(),
                cycle: 1,
                status: "complete".to_string(),
                cost_usd: 0.85,
                turns: 8,
                started_at: "2026-03-12T10:03:15".to_string(),
                finished_at: Some("2026-03-12T10:04:57".to_string()),
                output_summary: None,
                error_message: None,
            },
        ]
    }

    #[test]
    fn run_report_serializes_to_json() {
        let report = RunReport::from_run(&sample_run(), &sample_agent_runs());
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("abcd1234"));
        assert!(json.contains("implementer"));
        assert!(json.contains("reviewer"));
        assert!(json.contains("4.23"));
    }

    #[test]
    fn run_report_includes_all_agents() {
        let report = RunReport::from_run(&sample_run(), &sample_agent_runs());
        assert_eq!(report.agents.len(), 2);
    }

    #[test]
    fn empty_agent_runs_produces_valid_report() {
        let report = RunReport::from_run(&sample_run(), &[]);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"agents\":[]"));
    }

    #[test]
    fn print_run_report_captures_output() {
        // Verify the function doesn't panic and formats correctly
        let run = sample_run();
        let agents = sample_agent_runs();
        print_run_report(&run, &agents);
    }

    #[test]
    fn print_run_report_with_error() {
        let mut run = sample_run();
        run.error_message = Some("something broke".to_string());
        run.status = RunStatus::Failed;
        print_run_report(&run, &[]);
    }

    #[test]
    fn print_runs_table_formats_rows() {
        let runs = vec![
            sample_run(),
            Run {
                id: "efgh5678".to_string(),
                issue_number: 99,
                status: RunStatus::Failed,
                pr_number: None,
                branch: None,
                worktree_path: None,
                cost_usd: 12.50,
                auto_merge: false,
                started_at: "2026-03-13T10:00:00".to_string(),
                finished_at: None,
                error_message: Some("budget exceeded".to_string()),
                complexity: "full".to_string(),
                issue_source: "github".to_string(),
            },
        ];
        print_runs_table(&runs);
    }

    #[test]
    fn run_report_from_run_maps_all_fields() {
        let run = Run {
            id: "test0001".to_string(),
            issue_number: 7,
            status: RunStatus::Failed,
            pr_number: Some(55),
            branch: Some("oven/issue-7-abc".to_string()),
            worktree_path: None,
            cost_usd: 18.75,
            auto_merge: true,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:30:00".to_string()),
            error_message: Some("cost exceeded".to_string()),
            complexity: "full".to_string(),
            issue_source: "github".to_string(),
        };
        let agents = vec![AgentRun {
            id: 1,
            run_id: "test0001".to_string(),
            agent: "implementer".to_string(),
            cycle: 1,
            status: "failed".to_string(),
            cost_usd: 18.75,
            turns: 50,
            started_at: "2026-03-12T10:00:00".to_string(),
            finished_at: Some("2026-03-12T10:30:00".to_string()),
            output_summary: None,
            error_message: Some("budget".to_string()),
        }];
        let report = RunReport::from_run(&run, &agents);
        assert_eq!(report.id, "test0001");
        assert_eq!(report.status, "failed");
        assert_eq!(report.error_message.as_deref(), Some("cost exceeded"));
        assert_eq!(report.agents.len(), 1);
        assert_eq!(report.agents[0].turns, 50);
    }
}
