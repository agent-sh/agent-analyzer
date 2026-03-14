use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum RepoMapAction {
    /// Generate AST-based symbol map (not yet implemented)
    Generate {
        /// Repository path
        path: std::path::PathBuf,
    },
}

pub fn run(action: RepoMapAction) -> Result<()> {
    match action {
        RepoMapAction::Generate { .. } => {
            eprintln!("[WARN] repo-map is not yet implemented");
            println!("{{\"status\": \"not yet implemented\"}}");
            Ok(())
        }
    }
}
