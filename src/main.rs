//! flockboard — a herdr plugin that shows a live dashboard of Flock-managed
//! agent work: every agent in the herdr session with its status, and per-repo
//! open issues by Flock state label and open PRs, grouped by git-origin
//! organization. Read-only.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod git_org;
mod github;
mod herd;
mod state;
mod tui;

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
        Commands::Tui => tui::run(),
    }
}
