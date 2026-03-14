use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum CollectAction {
    /// Gather project data (not yet implemented)
    Run {
        /// Repository path
        path: std::path::PathBuf,
    },
}

pub fn run(action: CollectAction) -> Result<()> {
    match action {
        CollectAction::Run { .. } => {
            eprintln!("[WARN] collect is not yet implemented");
            println!("{{\"status\": \"not yet implemented\"}}");
            Ok(())
        }
    }
}
