//! Soak testing coordination for upgrade simulation.
//!
//! This module handles running soak-test workers alongside the rollup manager,
//! coordinating version transitions based on height polling.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use sov_rollup_manager::RunnerError;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::error::TestCaseError;
use crate::test_case::SoakTestingConfig;

/// Combined soak testing configuration and version binaries.
///
/// This type unifies `SoakTestingConfig` with the version-specific soak binaries
/// and their stop heights, ensuring they're always provided together.
#[derive(Debug, Clone)]
pub struct SoakManagerConfig {
    /// The soak testing configuration (worker count, salt, etc.).
    pub config: SoakTestingConfig,
    /// Soak binary paths paired with their stop heights.
    pub versions: Vec<(PathBuf, u64)>,
}

impl SoakManagerConfig {
    /// Create a new soak manager config.
    pub fn new(config: SoakTestingConfig, versions: Vec<(PathBuf, u64)>) -> Self {
        Self { config, versions }
    }

    /// Create the soak config for resync testing.
    ///
    /// During resync, the rollup replays existing transactions and won't accept
    /// new ones until it catches up. Soak testing only runs during the extra
    /// blocks phase at the end.
    ///
    /// Returns `None` if `extra_blocks_after_resync` is 0.
    pub fn for_resync(&self, extra_blocks_after_resync: u64) -> Option<Self> {
        if extra_blocks_after_resync == 0 {
            return None;
        }

        let (last_binary, last_stop_height) = self.versions.last().expect("has versions");
        let extended_stop_height = last_stop_height + extra_blocks_after_resync;

        // Increment salt to avoid transaction overlap with initial run
        let mut config = self.config.clone();
        config.salt += config.num_workers * self.versions.len() as u32;

        Some(Self {
            config,
            versions: vec![(last_binary.clone(), extended_stop_height)],
        })
    }
}

/// Wait for the rollup sequencer to be ready to accept transactions.
///
/// Polls `/sequencer/ready` endpoint until it returns a success response.
/// This is an unbounded loop intended to be raced against manager completion
/// via `tokio::select!`.
pub async fn wait_for_sequencer_ready(api_url: &str) -> Result<(), TestCaseError> {
    let client = reqwest::Client::new();
    let url = format!("{api_url}/sequencer/ready");

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                // Returns null when ready
                return Ok(());
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Poll rollup height until it reaches or exceeds the target height.
///
/// Uses async reqwest to query the rollup's height endpoint.
/// This is an unbounded loop intended to be raced against manager completion
/// via `tokio::select!`.
pub async fn wait_for_height(api_url: &str, target: u64) -> Result<(), TestCaseError> {
    let client = reqwest::Client::new();
    let url = format!("{api_url}/modules/chain-state/state/current-heights/");

    loop {
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                // Response format: { "value": [rollup_height, visible_slot_number] }
                if let Some(height) = json
                    .get("value")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_u64())
                {
                    if height >= target {
                        return Ok(());
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Start a soak-test process with the given configuration.
pub fn start_soak_process(
    binary: &Path,
    api_url: &str,
    num_workers: u32,
    salt: u32,
) -> Result<Child, TestCaseError> {
    tokio::process::Command::new(binary)
        .args([
            "--api-url",
            api_url,
            "--num-workers",
            &num_workers.to_string(),
            "--salt",
            &salt.to_string(),
        ])
        .spawn()
        .map_err(TestCaseError::SoakTestStartFailed)
}

/// Stop a running soak-test process gracefully.
///
/// Sends SIGTERM and waits for graceful shutdown (up to 1 second),
/// then falls back to SIGKILL if necessary.
///
/// Returns an error if the process had already exited with a failure code
/// before we tried to stop it (indicating an unexpected crash).
pub async fn stop_soak_process(mut child: Child) -> Result<(), TestCaseError> {
    // First check if the process already exited (before we send any signal)
    match child.try_wait() {
        Ok(Some(status)) => {
            // Process already exited - check if it was an error
            if !status.success() {
                let exit_code = status.code().unwrap_or(-1);
                error!(
                    exit_code,
                    "Soak-test process exited with error before termination"
                );
                return Err(TestCaseError::SoakTestFailed { exit_code });
            }
            // Exited successfully on its own (unusual but ok)
            return Ok(());
        }
        Ok(None) => {
            // Process still running, proceed to stop it
        }
        Err(e) => {
            warn!(?e, "Error checking soak-test process status");
        }
    }

    let pid = match child.id() {
        Some(id) => Pid::from_raw(id as i32),
        None => return Ok(()), // Already exited
    };

    // Send SIGTERM for graceful shutdown
    if let Err(e) = kill(pid, Signal::SIGTERM) {
        warn!(?e, "Failed to send SIGTERM to soak-test process");
    }

    // Wait for graceful shutdown with timeout
    match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(status)) => {
            if !status.success() {
                // Non-zero exit after SIGTERM is expected (signal termination)
                info!(?status, "Soak-test process exited after SIGTERM");
            }
        }
        Ok(Err(e)) => {
            warn!(?e, "Error waiting for soak-test process");
        }
        Err(_) => {
            // Timeout - force kill
            warn!("Soak-test process did not exit gracefully after 1s, sending SIGKILL");
            let _ = kill(pid, Signal::SIGKILL);
            let _ = child.wait().await;
        }
    }

    Ok(())
}

/// Stop a soak process if one is running.
pub async fn stop_soak_process_if_running(
    soak_process: &mut Option<Child>,
) -> Result<(), TestCaseError> {
    if let Some(child) = soak_process.take() {
        stop_soak_process(child).await?;
    }
    Ok(())
}

/// Coordinate soak testing alongside the rollup manager.
///
/// Monitors height to detect version transitions, stopping and restarting
/// soak workers at each transition point.
pub async fn run_soak_coordinator(
    soak_config: &SoakManagerConfig,
    api_url: &str,
    manager_handle: &mut JoinHandle<Result<(), RunnerError>>,
) -> Result<(), TestCaseError> {
    let mut soak_process: Option<Child> = None;
    let mut current_salt = soak_config.config.salt;

    for (idx, (binary, stop_height)) in soak_config.versions.iter().enumerate() {
        // Wait for sequencer ready (or manager to fail)
        tokio::select! {
            biased;

            result = &mut *manager_handle => {
                // Manager completed (error or success)
                stop_soak_process_if_running(&mut soak_process).await?;
                return result
                    .map_err(TestCaseError::ManagerTaskPanic)?
                    .map_err(TestCaseError::Runner);
            }
            ready_result = wait_for_sequencer_ready(api_url) => {
                ready_result?;
            }
        }

        info!(
            version = idx,
            binary = %binary.display(),
            stop_height,
            salt = current_salt,
            "Sequencer ready, starting soak workers"
        );

        // Start soak workers
        soak_process = Some(start_soak_process(
            binary,
            api_url,
            soak_config.config.num_workers,
            current_salt,
        )?);

        // Poll height until stop_height (or manager completes)
        tokio::select! {
            biased;

            result = &mut *manager_handle => {
                // Manager completed
                stop_soak_process_if_running(&mut soak_process).await?;
                return result
                    .map_err(TestCaseError::ManagerTaskPanic)?
                    .map_err(TestCaseError::Runner);
            }
            _ = wait_for_height(api_url, *stop_height) => {
                info!(
                    version = idx,
                    stop_height,
                    "Reached stop height, transitioning to next version"
                );
            }
        }

        // Stop soak workers before version transition
        stop_soak_process_if_running(&mut soak_process).await?;
        current_salt += soak_config.config.num_workers; // Avoid tx overlap with new salt
    }

    // Wait for manager to finish
    let result = manager_handle.await;
    stop_soak_process_if_running(&mut soak_process).await?;

    result
        .map_err(TestCaseError::ManagerTaskPanic)?
        .map_err(TestCaseError::Runner)
}
