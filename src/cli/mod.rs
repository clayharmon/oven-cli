pub mod clean;
pub mod look;
pub mod off;
pub mod on;
pub mod prep;
pub mod report;
pub mod ticket;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "oven", about = "let 'em cook", version)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Args)]
pub struct GlobalOpts {
    /// Enable verbose output
    #[arg(global = true, short, long)]
    pub verbose: bool,
    /// Suppress non-essential output
    #[arg(global = true, short, long)]
    pub quiet: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Set up project (recipe.toml, agents, db)
    Prep(PrepArgs),
    /// Start the pipeline
    On(OnArgs),
    /// Stop a detached run
    Off,
    /// View pipeline logs
    Look(LookArgs),
    /// Show run details and costs
    Report(ReportArgs),
    /// Remove worktrees, logs, merged branches
    Clean(CleanArgs),
    /// Local issue management
    Ticket(TicketArgs),
}

#[derive(Args)]
pub struct PrepArgs {
    /// Overwrite existing config
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct OnArgs {
    /// Comma-separated issue numbers
    pub ids: Option<String>,
    /// Run in background
    #[arg(short, long)]
    pub detached: bool,
    /// Auto-merge PRs when done
    #[arg(short, long)]
    pub merge: bool,
}

#[derive(Args)]
pub struct LookArgs {
    /// Run ID to view
    pub run_id: Option<String>,
    /// Filter to a specific agent
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Args)]
pub struct ReportArgs {
    /// Run ID to report on
    pub run_id: Option<String>,
    /// Show all runs
    #[arg(long)]
    pub all: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CleanArgs {
    /// Only remove logs
    #[arg(long)]
    pub only_logs: bool,
    /// Only remove worktrees
    #[arg(long)]
    pub only_trees: bool,
    /// Only remove merged branches
    #[arg(long)]
    pub only_branches: bool,
}

#[derive(Args)]
pub struct TicketArgs {
    #[command(subcommand)]
    pub command: TicketCommands,
}

#[derive(Subcommand)]
pub enum TicketCommands {
    /// Create a local issue
    Create(TicketCreateArgs),
    /// List local issues
    List(TicketListArgs),
    /// View a local issue
    View(TicketViewArgs),
    /// Close a local issue
    Close(TicketCloseArgs),
}

#[derive(Args)]
pub struct TicketCreateArgs {
    /// Issue title
    pub title: String,
    /// Issue body
    #[arg(long)]
    pub body: Option<String>,
    /// Add o-ready label immediately
    #[arg(long)]
    pub ready: bool,
}

#[derive(Args)]
pub struct TicketListArgs {
    /// Filter by label
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Args)]
pub struct TicketViewArgs {
    /// Issue ID
    pub id: u32,
}

#[derive(Args)]
pub struct TicketCloseArgs {
    /// Issue ID
    pub id: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
