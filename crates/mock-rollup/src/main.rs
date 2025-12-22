use std::path::PathBuf;
use std::process::ExitCode;
use std::{fs, thread, time::Duration};

use clap::Parser;
use serde::Deserialize;
use tracing::{error, info};

use mock_rollup::DEFAULT_BLOCK_ADVANCE;

#[derive(Parser)]
#[command(name = "mock-rollup")]
#[command(about = "Mock rollup binary for integration testing")]
struct Cli {
    /// Path to the rollup config file (TOML)
    #[arg(long)]
    rollup_config_path: PathBuf,

    /// Block height at which to start processing (skips blocks before this)
    #[arg(long)]
    start_at_height: Option<u64>,

    /// Block height at which to stop processing and exit
    #[arg(long)]
    stop_at_height: Option<u64>,
}

/// Minimal rollup config - just what we need for the mock.
#[derive(Debug, Deserialize)]
struct RollupConfig {
    /// Path to the state file that tracks the current block height.
    state_file: PathBuf,
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();

    // Load rollup config
    let config_content = fs::read_to_string(&cli.rollup_config_path)
        .map_err(|e| format!("failed to read rollup config: {e}"))?;
    let config: RollupConfig = toml::from_str(&config_content)
        .map_err(|e| format!("failed to parse rollup config: {e}"))?;

    // Read current height from state file (0 if doesn't exist)
    let current_height: u64 = if config.state_file.exists() {
        let content = fs::read_to_string(&config.state_file)
            .map_err(|e| format!("failed to read state file: {e}"))?;
        content
            .trim()
            .parse()
            .map_err(|e| format!("failed to parse state file: {e}"))?
    } else {
        0
    };

    info!(current_height, "Current state height");

    // Validate start_at_height if provided
    if let Some(start_height) = cli.start_at_height {
        let expected = current_height + 1;
        if start_height != expected {
            return Err(format!(
                "start_at_height ({start_height}) != current_height + 1 ({expected})"
            ));
        }
        info!(start_height, "Starting at height");
    } else {
        info!("Starting from genesis");
    }

    // Determine final height and update state
    let final_height = if let Some(stop_height) = cli.stop_at_height {
        info!(stop_height, "Will stop at height");
        stop_height
    } else {
        // No stop height - simulate processing some blocks indefinitely
        let new_height = current_height + DEFAULT_BLOCK_ADVANCE;
        info!(new_height, "No stop height, advancing to");
        new_height
    };

    // Simulate block processing
    thread::sleep(Duration::from_millis(100));

    // Write final height to state file
    fs::write(&config.state_file, final_height.to_string())
        .map_err(|e| format!("failed to write state file: {e}"))?;

    info!(final_height, "Processed blocks, state now at height");
    info!("Mock rollup shutting down gracefully");

    Ok(())
}

fn main() -> ExitCode {
    // Only initialize logging if RUST_LOG is set, keeping tests quiet by default
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
