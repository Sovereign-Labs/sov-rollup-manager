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
    /// Number of blocks before each version stop height where soak workers
    /// should be terminated to avoid shutdown races.
    #[serde(default = "default_safety_stop_blocks")]
    pub safety_stop_blocks: u64,
}

fn default_safety_stop_blocks() -> u64 {
    3
}

impl SoakManagerConfig {
    /// Create a new soak manager config.
    pub fn new(
        config: SoakWorkerConfig,
        versions: Vec<(PathBuf, u64)>,
        safety_stop_blocks: u64,
    ) -> Self {
        Self {
            config,
            versions,
            safety_stop_blocks,
        }
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
            safety_stop_blocks: self.safety_stop_blocks,
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

/// Wait until the sequencer is ready and the height is strictly above
/// `previous_stop_height`.
///
/// This is used to detect that a rollup restart has fully completed before
/// launching the next soak binary.
async fn wait_for_sequencer_ready_and_height_gt(api_url: &str, previous_stop_height: u64) {
    let client = reqwest::Client::new();
    let ready_url = format!("{api_url}/sequencer/ready");
    let height_url = format!("{api_url}/modules/chain-state/state/current-heights/");

    loop {
        let is_ready = client
            .get(&ready_url)
            .send()
            .await
            .map(|resp| resp.status().is_success())
            .unwrap_or(false);

        if is_ready
            && fetch_height(&client, &height_url)
                .await
                .map(|height| height > previous_stop_height)
                .unwrap_or(false)
        {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn fetch_height(client: &reqwest::Client, url: &str) -> Option<u64> {
    let resp = client.get(url).send().await.ok()?;
    let json = resp.json::<serde_json::Value>().await.ok()?;
    json.get("value")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_u64())
}

/// Poll rollup height until it reaches at least `target`.
async fn wait_for_height(api_url: &str, target: u64) {
    let client = reqwest::Client::new();
    let url = format!("{api_url}/modules/chain-state/state/current-heights/");

    loop {
        if fetch_height(&client, &url)
            .await
            .is_some_and(|height| height >= target)
        {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Start a soak-test process.
fn start_soak_process(
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
async fn stop_soak_process(mut child: Child) -> Result<(), SoakManagerError> {
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
async fn stop_soak_process_if_running(
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
    let mut previous_stop_height = 0_u64;

    for (idx, (binary, stop_height)) in soak_config.versions.iter().enumerate() {
        tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                info!("Received shutdown signal, stopping soak coordinator");
                stop_soak_process_if_running(&mut soak_process).await?;
                return Ok(());
            }
            _ = wait_for_sequencer_ready_and_height_gt(api_url, previous_stop_height) => {}
        }

        info!(
            version = idx,
            binary = %binary.display(),
            previous_stop_height,
            stop_height,
            salt = current_salt,
            "Sequencer ready and height advanced, starting soak workers"
        );

        soak_process = Some(start_soak_process(
            binary,
            api_url,
            soak_config.config.num_workers,
            current_salt,
        )?);

        let soak_stop_height = stop_height.saturating_sub(soak_config.safety_stop_blocks);

        tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                info!("Received shutdown signal, stopping soak coordinator");
                stop_soak_process_if_running(&mut soak_process).await?;
                return Ok(());
            }
            _ = wait_for_height(api_url, soak_stop_height) => {
                info!(
                    version = idx,
                    configured_stop_height = stop_height,
                    safety_stop_blocks = soak_config.safety_stop_blocks,
                    soak_stop_height,
                    "Reached soak stop height, transitioning to next soak version"
                );
            }
        }

        stop_soak_process_if_running(&mut soak_process).await?;
        current_salt += soak_config.config.num_workers;
        previous_stop_height = *stop_height;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::thread;
    use tempfile::TempDir;

    fn write_executable_script(path: &Path, content: &str) {
        fs::write(path, content).expect("write script");
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("set executable permissions");
    }

    fn allocate_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        listener.local_addr().expect("local addr").port()
    }

    fn start_mock_api_server(
        port: u16,
        stop: Arc<AtomicBool>,
        height: Arc<AtomicU64>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set nonblocking listener");

            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0_u8; 2048];
                        let _ = stream.read(&mut buf);
                        let req = String::from_utf8_lossy(&buf);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/");

                        let body = if path.starts_with("/sequencer/ready") {
                            "null".to_string()
                        } else if path.starts_with("/modules/chain-state/state/current-heights/") {
                            let next_height = height.fetch_add(1, Ordering::Relaxed) + 1;
                            format!(r#"{{"value":[{next_height},0]}}"#)
                        } else {
                            "{}".to_string()
                        };

                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
    }

    #[test]
    fn resync_config_none_when_extra_blocks_zero() {
        let cfg = SoakManagerConfig::new(
            SoakWorkerConfig {
                num_workers: 5,
                salt: 0,
            },
            vec![(PathBuf::from("/tmp/soak-v0"), 100)],
            4,
        );
        assert!(cfg.for_resync(0).is_none());
    }

    #[test]
    fn resync_config_uses_last_version_and_increments_salt() {
        let cfg = SoakManagerConfig::new(
            SoakWorkerConfig {
                num_workers: 3,
                salt: 7,
            },
            vec![
                (PathBuf::from("/tmp/soak-v0"), 100),
                (PathBuf::from("/tmp/soak-v1"), 200),
            ],
            7,
        );

        let resync = cfg.for_resync(10).expect("resync config");
        assert_eq!(resync.versions.len(), 1);
        assert_eq!(resync.versions[0].0, PathBuf::from("/tmp/soak-v1"));
        assert_eq!(resync.versions[0].1, 210);
        assert_eq!(resync.config.salt, 13); // 7 + (3 workers * 2 versions)
        assert_eq!(resync.safety_stop_blocks, 7);
    }

    #[tokio::test]
    async fn coordinator_runs_end_to_end_with_mock_api() {
        let tmp = TempDir::new().expect("tmpdir");
        let soak_script = tmp.path().join("soak.sh");
        write_executable_script(
            &soak_script,
            r#"#!/bin/sh
trap 'exit 0' TERM INT QUIT
while true; do
  sleep 1
done
"#,
        );

        let port = allocate_port();
        let stop = Arc::new(AtomicBool::new(false));
        let height = Arc::new(AtomicU64::new(0));
        let handle = start_mock_api_server(port, Arc::clone(&stop), Arc::clone(&height));
        let api_url = format!("http://127.0.0.1:{port}");

        let cfg = SoakManagerConfig::new(
            SoakWorkerConfig {
                num_workers: 2,
                salt: 5,
            },
            vec![(soak_script.clone(), 3), (soak_script, 6)],
            1,
        );

        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let result = run_soak_coordinator(&cfg, &api_url, shutdown_rx).await;

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("join server thread");

        assert!(result.is_ok(), "coordinator should succeed: {result:?}");
    }

    #[tokio::test]
    async fn coordinator_honors_early_shutdown() {
        let cfg = SoakManagerConfig::new(
            SoakWorkerConfig {
                num_workers: 1,
                salt: 0,
            },
            vec![(PathBuf::from("/tmp/nonexistent"), 100)],
            0,
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _ = shutdown_tx.send(());

        let result = run_soak_coordinator(&cfg, "http://127.0.0.1:1", shutdown_rx).await;
        assert!(result.is_ok(), "shutdown should short-circuit coordinator");
    }
}
