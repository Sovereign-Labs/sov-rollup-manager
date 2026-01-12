//! Node orchestration - spawn and monitor rollup-manager processes.
//!
//! This module handles running multiple rollup-manager processes in parallel,
//! one for each node (master + replicas). Each process is spawned with its own
//! working directory and manager config.
//!
//! The node runner accepts a shutdown sender to notify when all nodes complete
//! or any node fails, allowing coordination with soak testing.

/// Base port for prometheus metrics. Each node gets base_port + node_index.
const METRICS_BASE_PORT: u16 = 9845;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use sov_rollup_manager::{ManagerConfig, RollupVersion};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::error::TestCaseError;

/// Registry for tracking spawned process PIDs.
///
/// This allows external code (like signal handlers) to terminate all spawned
/// processes for graceful shutdown on Ctrl+C.
///
/// Stores (name, pid) pairs where name is for logging (e.g., "master", "replica_0", "mock-da").
#[derive(Debug, Clone, Default)]
pub struct ProcessRegistry {
    pids: Arc<Mutex<Vec<(String, u32)>>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a process PID with a name for logging.
    pub fn register(&self, name: impl Into<String>, pid: u32) {
        if let Ok(mut pids) = self.pids.lock() {
            pids.push((name.into(), pid));
        }
    }

    /// Terminate all registered processes with SIGTERM.
    pub fn terminate_all(&self) {
        if let Ok(pids) = self.pids.lock() {
            for (name, pid) in pids.iter() {
                let nix_pid = Pid::from_raw(*pid as i32);
                info!(name, pid, "Sending SIGTERM to process");
                if let Err(e) = signal::kill(nix_pid, Signal::SIGTERM) {
                    // ESRCH means process already exited, which is fine
                    if e != nix::errno::Errno::ESRCH {
                        warn!(name, pid, error = %e, "Failed to send SIGTERM");
                    }
                }
            }
        }
    }

    /// Clear all registered PIDs.
    pub fn clear(&self) {
        if let Ok(mut pids) = self.pids.lock() {
            pids.clear();
        }
    }
}

/// Versions for all nodes in a test case.
///
/// `nodes[0]` is always master, `nodes[1..]` are replicas.
/// Single-node tests have exactly one entry.
#[derive(Debug, Clone)]
pub struct NodeVersions {
    /// RollupVersions for each node. nodes[0] = master, nodes[1..] = replicas.
    pub nodes: Vec<Vec<RollupVersion>>,
}

/// Result from running a single node.
struct NodeResult {
    node_name: String,
    result: Result<(), TestCaseError>,
}

/// Grace period for SIGTERM before tasks are aborted (which triggers SIGKILL via kill_on_drop).
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn and monitor rollup-manager processes for all nodes.
///
/// Each node gets its own rollup-manager process with:
/// - A serialized ManagerConfig written to a JSON file in run_dir
/// - The process spawned with `current_dir(run_dir)` so relative paths work
/// - Process monitoring for exit status
///
/// All nodes are started roughly simultaneously and monitored in parallel.
/// If any node fails, all other nodes are terminated (SIGTERM) and the error
/// is propagated.
///
/// # Arguments
/// * `manager_binary` - Path to the rollup-manager binary
/// * `versions` - Version configurations for all nodes (nodes[0] = master)
/// * `node_names` - Names for each node (for logging), matching indices in versions.nodes
/// * `extra_args` - Additional arguments passed through to rollup (e.g., `--genesis-path`)
/// * `run_dir` - Working directory for all nodes (configs use relative paths from here)
/// * `shutdown_tx` - Optional sender to notify when nodes complete or fail.
///   This allows coordination with soak testing - the receiver can stop soak
///   workers when nodes are done.
/// * `registry` - Process registry for tracking PIDs, allowing external cleanup on Ctrl+C.
pub async fn run_nodes(
    manager_binary: &Path,
    versions: &NodeVersions,
    node_names: &[String],
    extra_args: &[String],
    run_dir: &Path,
    shutdown_tx: Option<oneshot::Sender<()>>,
    registry: &ProcessRegistry,
) -> Result<(), TestCaseError> {
    let mut join_set: JoinSet<NodeResult> = JoinSet::new();
    let mut child_processes: Vec<(String, Child)> = Vec::new();

    // Spawn all node processes
    for (node_idx, node_versions) in versions.nodes.iter().enumerate() {
        let node_name = node_names
            .get(node_idx)
            .cloned()
            .unwrap_or_else(|| format!("node_{node_idx}"));

        let child = spawn_node(
            manager_binary,
            &node_name,
            node_idx,
            node_versions,
            extra_args,
            run_dir,
        )
        .await?;
        child_processes.push((node_name, child));
    }

    info!(
        num_nodes = child_processes.len(),
        "All rollup-manager processes spawned"
    );

    // Register PIDs before moving children into tasks.
    // This allows external code (signal handlers) to terminate processes on Ctrl+C.
    // We also need these for internal error handling - kill_on_drop only sends SIGKILL,
    // which doesn't give rollup-manager a chance to terminate its child rollup process.
    for (node_name, child) in &child_processes {
        if let Some(pid) = child.id() {
            registry.register(node_name.clone(), pid);
        }
    }

    // Move children into the JoinSet for monitoring
    for (node_name, child) in child_processes {
        let name_clone = node_name.clone();
        join_set.spawn(async move {
            let result = wait_for_node(&name_clone, child).await;
            NodeResult {
                node_name: name_clone,
                result,
            }
        });
    }

    // Monitor all nodes - fail fast on any error
    let mut first_error: Option<TestCaseError> = None;

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(node_result) => {
                if let Err(e) = node_result.result {
                    error!(
                        node = %node_result.node_name,
                        error = %e,
                        "Node failed"
                    );
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    // Send SIGTERM to all remaining processes to allow graceful shutdown.
                    // This gives rollup-manager a chance to terminate its child rollup process.
                    // Without this, kill_on_drop would send SIGKILL which can't be handled.
                    registry.terminate_all();
                    // Give processes time to shut down gracefully before aborting tasks
                    tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
                    join_set.abort_all();
                    break;
                } else {
                    info!(node = %node_result.node_name, "Node completed successfully");
                }
            }
            Err(join_error) => {
                error!(error = %join_error, "Node task panicked");
                if first_error.is_none() {
                    first_error = Some(TestCaseError::NodeTaskPanic {
                        node_type: "unknown".to_string(),
                        source: join_error,
                    });
                }
                registry.terminate_all();
                tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
                join_set.abort_all();
                break;
            }
        }
    }

    // Signal shutdown to soak coordinator (if channel provided)
    // This happens whether we succeeded or failed
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(()); // Ignore error if receiver dropped
    }

    // Return the first error encountered, if any
    if let Some(e) = first_error {
        return Err(e);
    }

    Ok(())
}

/// Spawn a single rollup-manager process for a node.
async fn spawn_node(
    manager_binary: &Path,
    node_name: &str,
    node_idx: usize,
    versions: &[RollupVersion],
    extra_args: &[String],
    run_dir: &Path,
) -> Result<Child, TestCaseError> {
    // Write manager config to JSON file
    let config_filename = format!("manager_config_{node_name}.json");
    let config_path = run_dir.join(&config_filename);

    let config = ManagerConfig {
        versions: versions.to_vec(),
    };
    let config_json =
        serde_json::to_string_pretty(&config).map_err(TestCaseError::SerializeManagerConfig)?;

    fs::write(&config_path, config_json).map_err(|e| TestCaseError::WriteManagerConfig {
        node_type: node_name.to_string(),
        source: e,
    })?;

    // Compute metrics port based on node index
    let metrics_port = METRICS_BASE_PORT + node_idx as u16;

    // Build command
    let mut cmd = Command::new(manager_binary);
    cmd.args(["-c", &config_filename, "--no-checkpoint-file", "--"])
        .args(extra_args)
        .args(["--metrics", &metrics_port.to_string()])
        .current_dir(run_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true); // Ensure child is killed if handle is dropped

    info!(
        node = %node_name,
        binary = %manager_binary.display(),
        config = %config_filename,
        metrics_port,
        run_dir = %run_dir.display(),
        "Spawning rollup-manager process"
    );

    let child = cmd.spawn().map_err(|e| TestCaseError::ManagerSpawn {
        node_type: node_name.to_string(),
        source: e,
    })?;

    info!(
        node = %node_name,
        pid = child.id().unwrap_or(0),
        "Rollup-manager process started"
    );

    Ok(child)
}

/// Wait for a node's rollup-manager process to complete.
async fn wait_for_node(node_name: &str, mut child: Child) -> Result<(), TestCaseError> {
    let status = child.wait().await.map_err(|e| TestCaseError::ManagerWait {
        node_type: node_name.to_string(),
        source: e,
    })?;

    if !status.success() {
        return Err(TestCaseError::ManagerNonZeroExit {
            node_type: node_name.to_string(),
            status,
        });
    }

    Ok(())
}

/// Build the rollup-manager binary.
///
/// Uses escargot to build the binary, which automatically handles workspace
/// discovery regardless of the current working directory.
pub fn build_manager_binary() -> Result<PathBuf, TestCaseError> {
    info!("Building rollup-manager binary");

    let cargo_build = escargot::CargoBuild::new()
        .package("sov-rollup-manager")
        .bin("sov-rollup-manager")
        .release()
        .run()
        .map_err(|e| {
            TestCaseError::BuildManager(std::io::Error::other(format!(
                "escargot build failed: {e}"
            )))
        })?;

    let binary_path = cargo_build.path().to_path_buf();

    info!(path = %binary_path.display(), "Rollup-manager binary built");

    Ok(binary_path)
}
