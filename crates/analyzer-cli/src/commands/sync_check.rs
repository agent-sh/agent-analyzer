use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum SyncCheckAction {
    /// Check doc-code sync status (not yet implemented)
    Check {
        /// Repository path
        path: std::path::PathBuf,
    },
}

pub fn run(action: SyncCheckAction) -> Result<()> {
    match action {
        SyncCheckAction::Check { .. } => {
            eprintln!("[WARN] sync-check is not yet implemented");
            println!("{{\"status\": \"not yet implemented\"}}");
            Ok(())
        }
    }
}
