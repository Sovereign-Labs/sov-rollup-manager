use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info};

use sov_rollup_manager::{CheckpointConfig, DEFAULT_HEIGHT_SLACK, ManagerConfig, run};

#[derive(Parser)]
#[command(name = "sov-rollup-manager")]
#[command(
    about = "Manager for handling hard fork upgrades using multiple versioned rollup binaries"
)]
struct Cli {
    /// Path to the manager config file (JSON); see README.md for the configuration format
    #[arg(short, long)]
    config: PathBuf,

    /// Path to the checkpoint file for tracking current rollup version across restarts.
    /// Required unless --no-checkpoint-file is specified.
    #[arg(short = 'f', long, required_unless_present = "no_checkpoint_file")]
    checkpoint_file: Option<PathBuf>,

    /// Disable checkpoint file usage: the manager will always start from version 0 and will not
    /// save the current version when running.
    /// Do not use with a production rollup! Restarting a node from version 0 over existing
    /// later-version state will fail to run.
    #[arg(long, conflicts_with = "checkpoint_file")]
    no_checkpoint_file: bool,

    /// Height monitoring slack in blocks. If the rollup exits within this many blocks of the
    /// stop height, it's considered successful and the upgrade to the next version proceeds. This
    /// handles fast resync scenarios where the rollup may exit before we can poll the final height.
    #[arg(long, default_value_t = DEFAULT_HEIGHT_SLACK)]
    height_slack: u64,

    /// Additional arguments to pass to rollup binaries
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

    // Build checkpoint config from CLI arguments
    let checkpoint_config = if let Some(path) = cli.checkpoint_file {
        CheckpointConfig::Enabled { path }
    } else {
        CheckpointConfig::Disabled
    };

    if let Err(e) = run(
        &config,
        &cli.rollup_args,
        cli.height_slack,
        checkpoint_config,
    ) {
        error!("Error running rollup versions: {e}");

        // Preserve the rollup's exit code if available
        if let Some(code) = e.exit_code() {
            return ExitCode::from(code as u8);
        }
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
