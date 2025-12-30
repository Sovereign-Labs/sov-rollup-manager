use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info};

use sov_rollup_manager::{ManagerConfig, run};

#[derive(Parser)]
#[command(name = "sov-rollup-manager")]
#[command(
    about = "Manager for handling hard fork upgrades using multiple versioned rollup binaries"
)]
struct Cli {
    /// Path to the manager config file (JSON)
    #[arg(short, long)]
    config: PathBuf,

    /// Additional arguments to pass to rollup binaries (after --)
    #[arg(last = true)]
    rollup_args: Vec<String>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let config = match ManagerConfig::load(&cli.config) {
        Ok(config) => config,
        Err(e) => {
            error!("Error loading config: {e}");
            return ExitCode::FAILURE;
        }
    };

    info!(versions = config.versions.len(), "Loaded config");

    for (i, version) in config.versions.iter().enumerate() {
        let start = version
            .start_height
            .map(|h| h.to_string())
            .unwrap_or_else(|| "genesis".to_string());
        let stop = version
            .stop_height
            .map(|h| h.to_string())
            .unwrap_or_else(|| "∞".to_string());

        info!(
            version = i,
            binary = %version.rollup_binary.display(),
            start,
            stop,
            "Configured version"
        );
    }

    if let Err(e) = run(&config, &cli.rollup_args) {
        error!("Error running rollup versions: {e}");

        // Preserve the rollup's exit code if available
        if let Some(code) = e.exit_code() {
            return ExitCode::from(code as u8);
        }
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
