use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use analyzer_core::output;

#[derive(Subcommand)]
pub enum CollectAction {
    /// Gather project metadata (README, CI, license, languages)
    Run {
        /// Repository path
        path: PathBuf,
    },
}

pub fn run(action: CollectAction) -> Result<()> {
    match action {
        CollectAction::Run { path } => {
            eprintln!("[INFO] Collecting project metadata from {}", path.display());
            let metadata = analyzer_collectors::collect_metadata(&path)?;
            println!("{}", output::to_json(&metadata));
            eprintln!("[OK] Project metadata collected");
            Ok(())
        }
    }
}
