//! Runner for executing rollup versions with height monitoring.

use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use signal_hook::consts::{SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::checkpoint::{
    Checkpoint, CheckpointConfig, CheckpointError, load_checkpoint, write_checkpoint,
};
use crate::config::{ManagerConfig, RollupVersion};
use crate::rollup_api::{RollupApiClient, RollupApiError};

/// Interval between height polling attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Grace period for SIGTERM before escalating to SIGKILL.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("failed to spawn binary '{path}': {source}")]
    Spawn {
        path: String,
        source: std::io::Error,
    },

    #[error("binary '{path}' exited with non-zero status: {status}")]
    NonZeroExit { path: String, status: ExitStatus },

    #[error("failed to wait for binary '{path}': {source}")]
    Wait {
        path: String,
        source: std::io::Error,
    },

    #[error("failed to initialize rollup API client: {0}")]
    ApiClientInit(#[from] RollupApiError),

    #[error(
        "rollup exited prematurely at height {current_height} \
         (expected to run until {stop_height})"
    )]
    PrematureExit {
        current_height: u64,
        stop_height: u64,
    },

    #[error(
        "rollup exceeded stop height: current height {current_height} > stop height {stop_height}"
    )]
    ExceededStopHeight {
        current_height: u64,
        stop_height: u64,
    },

    #[error("rollup exited but height could not be determined (stop_height was {stop_height})")]
    UnknownExitHeight { stop_height: u64 },

    #[error(
        "rollup exited unexpectedly without a configured stop height (last known height: {last_known_height:?})"
    )]
    UnexpectedExit { last_known_height: Option<u64> },

    #[error("failed to terminate rollup process: {0}")]
    Terminate(std::io::Error),

    #[error("failed to send {signal} to rollup process: {source}")]
    Signal {
        signal: &'static str,
        source: nix::Error,
    },

    #[error("failed to register signal handler: {0}")]
    SignalRegistration(#[from] std::io::Error),

    #[error("failed to read checkpoint file: {0}")]
    CheckpointRead(#[source] CheckpointError),

    #[error("failed to write checkpoint file: {0}")]
    CheckpointWrite(#[source] CheckpointError),

    #[error("checkpoint version index {index} is out of bounds (config has {count} versions)")]
    CheckpointIndexOutOfBounds { index: usize, count: usize },

    #[error(
        "checkpoint binary path mismatch at version {index}: \
         checkpoint has {checkpoint_path}, config has {config_path}"
    )]
    CheckpointBinaryMismatch {
        index: usize,
        checkpoint_path: PathBuf,
        config_path: PathBuf,
    },
}

impl RunnerError {
    /// Returns the exit code from the failed rollup process, if available.
    ///
    /// This is only available for `NonZeroExit` errors where the process
    /// exited normally (not terminated by a signal).
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            RunnerError::NonZeroExit { status, .. } => status.code(),
            _ => None,
        }
    }
}

/// Default slack for height monitoring (blocks).
pub const DEFAULT_HEIGHT_SLACK: u64 = 10;

/// Runs all rollup versions in sequence.
///
/// # Arguments
/// * `config` - The manager configuration with version definitions
/// * `extra_args` - Additional arguments passed to every rollup binary invocation
/// * `height_slack` - Number of blocks before stop height that's still considered successful.
///   This handles fast resync scenarios where the rollup may exit before we can poll
///   the final height. Use `DEFAULT_HEIGHT_SLACK` for the recommended value.
/// * `checkpoint_config` - Checkpoint file configuration
pub fn run(
    config: &ManagerConfig,
    extra_args: &[String],
    height_slack: u64,
    checkpoint_config: CheckpointConfig,
) -> Result<(), RunnerError> {
    let num_versions = config.versions.len();

    // Load and validate checkpoint if enabled
    let start_index = match &checkpoint_config {
        CheckpointConfig::Enabled { path } => {
            match load_checkpoint(path).map_err(RunnerError::CheckpointRead)? {
                Some(checkpoint) => {
                    // Validate checkpoint is within bounds
                    if checkpoint.version_index >= num_versions {
                        return Err(RunnerError::CheckpointIndexOutOfBounds {
                            index: checkpoint.version_index,
                            count: num_versions,
                        });
                    }

                    // Validate binary path matches
                    let config_binary = &config.versions[checkpoint.version_index].rollup_binary;
                    if checkpoint.binary_path != *config_binary {
                        return Err(RunnerError::CheckpointBinaryMismatch {
                            index: checkpoint.version_index,
                            checkpoint_path: checkpoint.binary_path,
                            config_path: config_binary.clone(),
                        });
                    }

                    info!(
                        version_index = checkpoint.version_index,
                        binary = %checkpoint.binary_path.display(),
                        "Resuming from checkpoint"
                    );
                    checkpoint.version_index
                }
                None => {
                    info!("No checkpoint file found, starting from version 0");
                    0
                }
            }
        }
        CheckpointConfig::Disabled => {
            info!("Checkpoint file disabled, starting from version 0");
            0
        }
    };

    for (i, version) in config.versions.iter().enumerate() {
        // Skip versions before the checkpoint
        if i < start_index {
            info!(
                version = i,
                binary = %version.rollup_binary.display(),
                "Skipping already-completed version"
            );
            continue;
        }

        let is_last = i == num_versions - 1;

        // Write checkpoint before launching this version
        if let CheckpointConfig::Enabled { path } = &checkpoint_config {
            let checkpoint = Checkpoint {
                version_index: i,
                binary_path: version.rollup_binary.clone(),
            };
            write_checkpoint(path, &checkpoint).map_err(RunnerError::CheckpointWrite)?;
            info!(
                version = i,
                checkpoint_file = %path.display(),
                "Updated checkpoint file"
            );
        }

        info!(
            version = i,
            binary = %version.rollup_binary.display(),
            "Starting version"
        );

        if is_last && version.stop_height.is_some() {
            warn!(
                stop_height = version.stop_height,
                "Last version has a stop height configured. The rollup will exit \
                 after this height is reached rather than running indefinitely."
            );
        }

        // Run migration if specified
        if let Some(ref migration_path) = version.migration_path {
            info!(migration = %migration_path.display(), "Running migration");
            run_migration(migration_path.to_str().unwrap())?;
        }

        // Build arguments for the rollup binary
        let mut args = build_rollup_args(version);
        args.extend(extra_args.iter().cloned());

        // Run the rollup with height monitoring
        run_version_with_monitoring(version, &args, height_slack)?;

        info!(version = i, "Version completed successfully");
    }

    info!("All versions completed");
    Ok(())
}

fn build_rollup_args(version: &RollupVersion) -> Vec<String> {
    let mut args = vec![
        "--rollup-config-path".to_string(),
        version.config_path.to_string_lossy().into_owned(),
    ];

    if let Some(start) = version.start_height {
        args.push("--start-at-rollup-height".to_string());
        args.push(start.to_string());
    }

    if let Some(stop) = version.stop_height {
        args.push("--stop-at-rollup-height".to_string());
        args.push(stop.to_string());
    }

    args
}

/// Run a migration binary (simple spawn and wait).
fn run_migration(path: &str) -> Result<(), RunnerError> {
    let mut child = Command::new(path).spawn().map_err(|e| RunnerError::Spawn {
        path: path.to_string(),
        source: e,
    })?;

    let status = child.wait().map_err(|e| RunnerError::Wait {
        path: path.to_string(),
        source: e,
    })?;

    if !status.success() {
        return Err(RunnerError::NonZeroExit {
            path: path.to_string(),
            status,
        });
    }

    Ok(())
}

/// Run a rollup version with height monitoring.
///
/// This spawns the rollup process and polls its height endpoint to ensure
/// it reaches the expected stop height before exiting.
fn run_version_with_monitoring(
    version: &RollupVersion,
    args: &[String],
    height_slack: u64,
) -> Result<(), RunnerError> {
    let binary_path = version.rollup_binary.to_str().unwrap();

    // Create API client for height queries
    let api_client = RollupApiClient::from_config(&version.config_path)?;

    // Register signal handlers for forwarding to child
    let mut signals = Signals::new([SIGTERM, SIGQUIT])?;

    // Spawn the rollup process
    let mut child =
        Command::new(binary_path)
            .args(args)
            .spawn()
            .map_err(|e| RunnerError::Spawn {
                path: binary_path.to_string(),
                source: e,
            })?;

    info!(pid = child.id(), "Spawned rollup process");

    // Run the monitoring loop
    let result = monitor_rollup(
        &mut child,
        &api_client,
        &mut signals,
        version.stop_height,
        binary_path,
        height_slack,
    );

    // Ensure process is cleaned up on error
    if result.is_err() {
        let _ = terminate_process(&mut child);
    }

    result
}

/// Monitor a running rollup process, polling height and handling exit conditions.
fn monitor_rollup(
    child: &mut Child,
    api_client: &RollupApiClient,
    signals: &mut Signals,
    stop_height: Option<u64>,
    binary_path: &str,
    height_slack: u64,
) -> Result<(), RunnerError> {
    let mut last_known_height: Option<u64> = None;
    let pid = Pid::from_raw(child.id() as i32);

    loop {
        // Check for signals to forward to the child process
        for sig in signals.pending() {
            let signal = match sig {
                SIGTERM => Signal::SIGTERM,
                SIGQUIT => Signal::SIGQUIT,
                _ => continue,
            };
            info!(?signal, "Forwarding signal to rollup process");
            if let Err(e) = signal::kill(pid, signal) {
                warn!(error = %e, ?signal, "Failed to forward signal");
            }
        }

        // Try to query height (may fail if API not ready or process crashed)
        match api_client.query_rollup_height() {
            Ok(height) => {
                last_known_height = Some(height);
                debug!(height, "Polled rollup height");

                // Check if exceeded stop height
                if let Some(stop) = stop_height {
                    if height > stop {
                        error!(
                            current_height = height,
                            stop_height = stop,
                            "Rollup exceeded configured stop height! Something is very wrong. Terminating rollup, manual intervention will be required for this upgrade."
                        );
                        terminate_process(child)?;
                        return Err(RunnerError::ExceededStopHeight {
                            current_height: height,
                            stop_height: stop,
                        });
                    }
                }
            }
            Err(e) => {
                // API not ready or request failed - this is expected during startup
                // or on shutdown
                warn!(error = %e, "Height query failed");
            }
        }

        // Check if process exited
        match child.try_wait() {
            Ok(Some(status)) => {
                return handle_process_exit(
                    status,
                    last_known_height,
                    stop_height,
                    binary_path,
                    height_slack,
                );
            }
            Ok(None) => {
                // Process still running, continue monitoring
            }
            Err(e) => {
                return Err(RunnerError::Wait {
                    path: binary_path.to_string(),
                    source: e,
                });
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

/// Handle the rollup process exit, validating the final height.
///
/// The `height_slack` parameter controls how many blocks before the stop height
/// is still considered successful. This is useful for fast resync scenarios where
/// the rollup may exit before we can poll the final height.
fn handle_process_exit(
    status: ExitStatus,
    last_known_height: Option<u64>,
    stop_height: Option<u64>,
    binary_path: &str,
    height_slack: u64,
) -> Result<(), RunnerError> {
    info!(
        ?status,
        ?last_known_height,
        height_slack,
        "Rollup process exited"
    );

    // If process exited with error, always propagate it
    if !status.success() {
        return Err(RunnerError::NonZeroExit {
            path: binary_path.to_string(),
            status,
        });
    }

    // Process exited successfully - validate height
    let Some(stop) = stop_height else {
        // No stop height specified but rollup exited - this shouldn't happen.
        // The rollup should run indefinitely unless given a stop height.
        error!(
            ?last_known_height,
            "Rollup exited unexpectedly without a configured stop height"
        );
        return Err(RunnerError::UnexpectedExit { last_known_height });
    };

    let Some(height) = last_known_height else {
        // We never got a height reading - this shouldn't happen normally.
        // It IS possible if the version's runtime is very short (a few blocks) and we are
        // resyncing past blocks very fast. This is very unlikely to come up in production. TODO:
        // add a check for this.
        error!(
            stop_height = stop,
            "Rollup exited but height was never determined"
        );
        return Err(RunnerError::UnknownExitHeight { stop_height: stop });
    };

    // Calculate the minimum acceptable height based on slack
    let min_acceptable = stop.saturating_sub(height_slack);

    if height >= min_acceptable && height <= stop {
        // Height is within acceptable range (stop - slack to stop)
        if height == stop {
            info!(height, "Rollup completed at expected stop height");
        } else {
            info!(
                height,
                stop_height = stop,
                height_slack,
                "Rollup completed within acceptable height range"
            );
        }
        Ok(())
    } else if height < min_acceptable {
        // Premature exit - height is below the acceptable range
        error!(
            current_height = height,
            stop_height = stop,
            min_acceptable,
            "Rollup exited prematurely (below acceptable height range)"
        );
        Err(RunnerError::PrematureExit {
            current_height: height,
            stop_height: stop,
        })
    } else {
        // height > stop - should have been caught during monitoring, but handle it anyway
        error!(
            current_height = height,
            stop_height = stop,
            "Rollup exceeded stop height"
        );
        Err(RunnerError::ExceededStopHeight {
            current_height: height,
            stop_height: stop,
        })
    }
}

/// Terminate a process gracefully (SIGTERM), then forcefully (SIGKILL) if needed.
fn terminate_process(child: &mut Child) -> Result<(), RunnerError> {
    let pid = Pid::from_raw(child.id() as i32);

    info!(%pid, "Sending SIGTERM to rollup process");
    signal::kill(pid, Signal::SIGTERM).map_err(|e| RunnerError::Signal {
        signal: "SIGTERM",
        source: e,
    })?;

    // Wait for graceful shutdown with timeout
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                info!(?status, "Rollup process terminated gracefully");
                return Ok(());
            }
            Ok(None) => {
                if start.elapsed() >= GRACEFUL_SHUTDOWN_TIMEOUT {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(RunnerError::Terminate(e)),
        }
    }

    // Graceful shutdown timed out, force kill
    warn!(%pid, "Graceful shutdown timed out, sending SIGKILL");
    signal::kill(pid, Signal::SIGKILL).map_err(|e| RunnerError::Signal {
        signal: "SIGKILL",
        source: e,
    })?;
    child.wait().map_err(RunnerError::Terminate)?;

    info!("Rollup process killed");
    Ok(())
}
