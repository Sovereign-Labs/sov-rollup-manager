//! Soak testing coordination for versioned rollup execution.
//!
//! This crate provides a library-first API for running soak binaries across
//! multiple rollup versions. It keeps state in-memory and does not require
//! config files on disk.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Child;
use tokio::sync::oneshot;
use tracing::{error, info, warn};

/// Worker options shared across versioned soak runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakWorkerConfig {
    /// Number of concurrent workers.
    pub num_workers: u32,
    /// RNG salt used by workers.
    pub salt: u32,
}

/// Soak manager configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakManagerConfig {
    /// Worker config (count + base salt).
    pub config: SoakWorkerConfig,
    /// Soak binary paths paired with stop heights.
    pub versions: Vec<(PathBuf, u64)>,
}

impl SoakManagerConfig {
    /// Create a new soak manager config.
    pub fn new(config: SoakWorkerConfig, versions: Vec<(PathBuf, u64)>) -> Self {
        Self { config, versions }
    }

    /// Create a config for resync testing.
    ///
    /// During resync, only the last version is relevant. We extend its stop
    /// height by `extra_blocks_after_resync`.
    pub fn for_resync(&self, extra_blocks_after_resync: u64) -> Option<Self> {
        if extra_blocks_after_resync == 0 {
            return None;
        }

        let (last_binary, last_stop_height) = self.versions.last()?;
        let extended_stop_height = last_stop_height + extra_blocks_after_resync;

        let mut config = self.config.clone();
        config.salt += config.num_workers * self.versions.len() as u32;

        Some(Self {
            config,
            versions: vec![(last_binary.clone(), extended_stop_height)],
        })
    }
}

#[derive(Debug, Error)]
pub enum SoakManagerError {
    #[error("failed to start soak process: {0}")]
    SoakTestStartFailed(std::io::Error),

    #[error("soak process failed with exit code {exit_code}")]
    SoakTestFailed { exit_code: i32 },
}

/// Wait for the rollup sequencer to be ready.
pub async fn wait_for_sequencer_ready(api_url: &str) {
    let client = reqwest::Client::new();
    let url = format!("{api_url}/sequencer/ready");

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Poll rollup height until it reaches at least `target`.
pub async fn wait_for_height(api_url: &str, target: u64) {
    let client = reqwest::Client::new();
    let url = format!("{api_url}/modules/chain-state/state/current-heights/");

    loop {
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(height) = json
                    .get("value")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_u64())
                {
                    if height >= target {
                        return;
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Start a soak-test process.
pub fn start_soak_process(
    binary: &Path,
    api_url: &str,
    num_workers: u32,
    salt: u32,
) -> Result<Child, SoakManagerError> {
    tokio::process::Command::new(binary)
        .args([
            "--api-url",
            api_url,
            "--num-workers",
            &num_workers.to_string(),
            "--salt",
            &salt.to_string(),
        ])
        .kill_on_drop(true)
        .spawn()
        .map_err(SoakManagerError::SoakTestStartFailed)
}

/// Stop a running soak-test process gracefully.
pub async fn stop_soak_process(mut child: Child) -> Result<(), SoakManagerError> {
    match child.try_wait() {
        Ok(Some(status)) => {
            if !status.success() {
                let exit_code = status.code().unwrap_or(-1);
                error!(
                    exit_code,
                    "Soak-test process exited with error before termination"
                );
                return Err(SoakManagerError::SoakTestFailed { exit_code });
            }
            return Ok(());
        }
        Ok(None) => {}
        Err(e) => {
            warn!(?e, "Error checking soak-test process status");
        }
    }

    let pid = match child.id() {
        Some(id) => Pid::from_raw(id as i32),
        None => return Ok(()),
    };

    if let Err(e) = kill(pid, Signal::SIGTERM) {
        warn!(?e, "Failed to send SIGTERM to soak-test process");
    }

    match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(status)) => {
            if !status.success() {
                info!(?status, "Soak-test process exited after SIGTERM");
            }
        }
        Ok(Err(e)) => {
            warn!(?e, "Error waiting for soak-test process");
        }
        Err(_) => {
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
) -> Result<(), SoakManagerError> {
    if let Some(child) = soak_process.take() {
        stop_soak_process(child).await?;
    }
    Ok(())
}

/// Coordinate soak testing alongside rollup nodes.
pub async fn run_soak_coordinator(
    soak_config: &SoakManagerConfig,
    api_url: &str,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), SoakManagerError> {
    let mut soak_process: Option<Child> = None;
    let mut current_salt = soak_config.config.salt;

    for (idx, (binary, stop_height)) in soak_config.versions.iter().enumerate() {
        tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                info!("Received shutdown signal, stopping soak coordinator");
                stop_soak_process_if_running(&mut soak_process).await?;
                return Ok(());
            }
            _ = wait_for_sequencer_ready(api_url) => {}
        }

        info!(
            version = idx,
            binary = %binary.display(),
            stop_height,
            salt = current_salt,
            "Sequencer ready, starting soak workers"
        );

        soak_process = Some(start_soak_process(
            binary,
            api_url,
            soak_config.config.num_workers,
            current_salt,
        )?);

        tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                info!("Received shutdown signal, stopping soak coordinator");
                stop_soak_process_if_running(&mut soak_process).await?;
                return Ok(());
            }
            _ = wait_for_height(api_url, *stop_height) => {
                info!(
                    version = idx,
                    stop_height,
                    "Reached stop height, transitioning to next soak version"
                );
            }
        }

        stop_soak_process_if_running(&mut soak_process).await?;
        current_salt += soak_config.config.num_workers;
    }

    info!("Soak testing completed all versions, waiting for nodes to finish");
    tokio::select! {
        _ = &mut shutdown_rx => {
            info!("Received shutdown signal after soak completion");
        }
        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
    }

    stop_soak_process_if_running(&mut soak_process).await?;
    Ok(())
}
