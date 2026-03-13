#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use clap::Parser;
use oven_cli::cli::{self, Cli, Commands};

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Prep(args) => cli::prep::run(args, &cli.global).await,
        Commands::On(args) => cli::on::run(args, &cli.global).await,
        Commands::Off => cli::off::run(&cli.global).await,
        Commands::Look(args) => cli::look::run(args, &cli.global).await,
        Commands::Report(args) => cli::report::run(args, &cli.global).await,
        Commands::Clean(args) => cli::clean::run(args, &cli.global).await,
        Commands::Ticket(args) => cli::ticket::run(args, &cli.global).await,
    }
}
