use clap::{Parser, Subcommand};

mod commands;

#[derive(Parser)]
#[command(
    name = "agent-analyzer",
    version,
    about = "Static analysis for the agent-sh ecosystem"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Repository intelligence - extraction, aggregation, and queries
    RepoIntel {
        #[command(subcommand)]
        action: commands::repo_intel::RepoIntelAction,
    },
    /// AST-based repository symbol mapping (not yet implemented)
    RepoMap {
        #[command(subcommand)]
        action: commands::repo_map::RepoMapAction,
    },
    /// Project data gathering (not yet implemented)
    Collect {
        #[command(subcommand)]
        action: commands::collect::CollectAction,
    },
    /// Doc-code sync analysis (not yet implemented)
    SyncCheck {
        #[command(subcommand)]
        action: commands::sync_check::SyncCheckAction,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::RepoIntel { action } => commands::repo_intel::run(action),
        Commands::RepoMap { action } => commands::repo_map::run(action),
        Commands::Collect { action } => commands::collect::run(action),
        Commands::SyncCheck { action } => commands::sync_check::run(action),
    };

    if let Err(e) = result {
        eprintln!("[ERROR] {e:#}");
        std::process::exit(1);
    }
}
