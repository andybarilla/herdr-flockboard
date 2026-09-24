//! flockboard — a herdr plugin that shows a live dashboard of Flock-managed
//! agent work: issues by workflow state, running agents and their stage, and
//! what is waiting on the human.
//!
//! Scaffold only for now — the `tui` command is a stub. See ROADMAP.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "flockboard",
    version,
    about = "Live dashboard for Flock-managed agent work in a herdr session"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the dashboard TUI.
    Tui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Tui => {
            eprintln!("flockboard: dashboard TUI not implemented yet (scaffold build).");
            std::process::exit(1);
        }
    }
}
