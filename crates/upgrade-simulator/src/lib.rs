//! Integration test framework for testing sov-rollup-manager with real Sovereign SDK rollups.
//!
//! This framework manages:
//! - Git repository cloning and checkout of specific commits
//! - Building rollup binaries and caching them by commit hash
//! - Constructing manager configs and running upgrade test cases
//! - Resync testing: clearing state and replaying from DA data
//! - Soak testing: generating transaction load during upgrade tests
//! - Multi-node replication testing with postgres docker containers

mod builder;
mod docker;
mod error;
mod node_config;
mod node_runner;
mod soak;
mod test_case;

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::{process::Child, sync::oneshot};
use tracing::{error, info, warn};

pub use builder::{BuilderError, DEFAULT_REPO_URL, RollupBuilder};
pub use error::TestCaseError;
pub use test_case::{LoadTestCaseError, SoakTestingConfig, TestCase, VersionSpec, load_test_case};

use docker::PostgresDockerContainer;
use node_config::{detect_node_layout, validate_node_layout};
use node_runner::{NodeVersions, ProcessRegistry, build_manager_binary, run_nodes};
use soak::{SoakManagerConfig, run_soak_coordinator};

/// Grace period after Ctrl+C for processes to shut down gracefully.
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Default port for the mock-da-server gRPC endpoint.
const MOCK_DA_PORT: u16 = 50051;

/// Run a test case through the rollup manager.
///
/// The config directory is derived as `{test_root}/{test_case.name}`.
/// A temporary run directory is created for rollup state and DA data.
///
/// After a successful initial run, the storage directory is cleared and the test
/// is re-run to verify resync functionality (replaying from DA data).
/// The entire run directory is cleaned up on success.
///
/// For multi-node tests (with replicas), a postgres docker container is
/// automatically started and cleaned up.
///
/// If soak testing is configured, transaction load is generated alongside
/// the rollup during the test.
///
/// This function handles Ctrl+C (SIGINT) gracefully by:
/// 1. Sending SIGTERM to all spawned processes
/// 2. Waiting for graceful shutdown
/// 3. Cleaning up the Docker container
pub async fn run_test_case(
    cache_dir: &Path,
    test_root: &Path,
    test_case: &TestCase,
) -> Result<(), TestCaseError> {
    info!(name = %test_case.name, repo_url = %test_case.repo_url, "Running test case");

    // Create builder with the test case's repo URL
    let builder = RollupBuilder::with_repo_url(cache_dir.to_path_buf(), test_case.repo_url.clone());

    let test_case_root = test_root.join(&test_case.name);
    let test_case_root = test_case_root
        .canonicalize()
        .map_err(|e| TestCaseError::InvalidTestCaseRoot(test_case_root.clone(), e))?;

    // Detect node layout (single node vs master + replicas)
    let layout = detect_node_layout(&test_case_root, test_case.versions.len())?;
    validate_node_layout(&layout)?;

    info!(
        num_replicas = layout.num_replicas(),
        requires_postgres = layout.requires_postgres,
        "Detected node layout"
    );

    // Create temporary run directory for rollup state and DA data
    let run_dir = test_case_root.join("run-dir");
    if run_dir.exists() {
        fs::remove_dir_all(&run_dir).map_err(|e| TestCaseError::ClearRunDir(run_dir.clone(), e))?;
    }
    fs::create_dir_all(&run_dir).map_err(|e| TestCaseError::CreateRunDir(run_dir.clone(), e))?;
    info!(run_dir = %run_dir.display(), "Created run directory");

    let mut postgres_container = PostgresDockerContainer::start()?;

    // Build rollup-manager binary using escargot (handles workspace discovery automatically)
    let manager_binary = build_manager_binary()?;

    // Build node versions for all nodes
    let versions = test_case.build_node_versions(&builder, &layout)?;

    // Build mock-da URL for config interpolation (nodes connect to mock-da-server at this URL)
    let mock_da_url = format!("http://localhost:{MOCK_DA_PORT}");

    // Interpolate config files with connection strings and write to run_dir
    let versions = interpolate_node_configs(
        versions,
        &run_dir,
        &postgres_container.sequencer_connection_string(),
        &mock_da_url,
    )?;

    // Build soak manager config if soak testing is enabled
    let soak_manager_config = test_case.build_soak_config(&builder)?;

    // Get master's API URL for soak testing
    let master_api_url = format!("http://localhost:{}", layout.master.http_port);

    // Genesis file is expected at {test_case_root}/genesis.json
    let genesis_path = test_case_root.join("genesis.json");
    let extra_args = vec![
        "--genesis-path".to_string(),
        genesis_path.to_string_lossy().into_owned(),
    ];

    // Create storage directories for all nodes
    for node in layout.all_nodes() {
        let storage_in_run_dir = run_dir.join(&node.storage_path);
        clear_storage_dir(&storage_in_run_dir)?;
    }

    // Create process registry for signal handling
    let registry = ProcessRegistry::new();

    // Build and spawn mock-da-server (runs for entire test, DA data persists across phases)
    let mock_da_binary = builder
        .get_mock_da_binary(&test_case.versions[0].commit)
        .map_err(TestCaseError::Build)?;
    let _mock_da_handle = spawn_mock_da_server(
        &mock_da_binary,
        &postgres_container.da_connection_string(),
        MOCK_DA_PORT,
        &registry,
    )
    .await?;

    // Run the test with signal handling
    let result = tokio::select! {
        biased;

        // Check for Ctrl+C first (biased ensures this is checked before test progress)
        _ = tokio::signal::ctrl_c() => {
            warn!("Ctrl+C received, shutting down gracefully...");
            registry.terminate_all();
            tokio::time::sleep(SHUTDOWN_GRACE_PERIOD).await;
            Err(TestCaseError::Interrupted)
        }

        // Run the actual test
        result = run_test_phases(
            test_case,
            &manager_binary,
            &versions,
            &extra_args,
            &run_dir,
            soak_manager_config.as_ref(),
            &master_api_url,
            &layout,
            &mut postgres_container,
            &registry,
        ) => result
    };

    // Terminate all remaining processes (mock-da-server, any lingering nodes)
    // This is a no-op if processes already exited
    registry.terminate_all();

    // Always clean up postgres container
    if let Err(e) = postgres_container.cleanup() {
        error!(error = %e, "Failed to cleanup postgres container");
    }

    result
}

/// Run the test phases (initial run + resync).
///
/// This is separated from `run_test_case` to allow signal handling to wrap it.
#[allow(clippy::too_many_arguments)]
async fn run_test_phases(
    test_case: &TestCase,
    manager_binary: &Path,
    versions: &NodeVersions,
    extra_args: &[String],
    run_dir: &Path,
    soak_manager_config: Option<&SoakManagerConfig>,
    master_api_url: &str,
    layout: &node_config::TestNodeLayout,
    postgres_container: &mut PostgresDockerContainer,
    registry: &ProcessRegistry,
) -> Result<(), TestCaseError> {
    // Run initial test
    run_with_soak(
        manager_binary,
        versions,
        extra_args,
        run_dir,
        soak_manager_config,
        master_api_url,
        registry,
    )
    .await?;

    info!(name = %test_case.name, "Initial run completed successfully");

    // Clear registry for resync phase (processes from initial run are done)
    registry.clear();

    // Resync test: clear storage and re-run (DA data persists in run_dir)
    info!(
        name = %test_case.name,
        extra_blocks = test_case.extra_blocks_after_resync,
        "Starting resync test"
    );

    // Clear storage for all nodes
    for node in layout.all_nodes() {
        let storage_in_run_dir = run_dir.join(&node.storage_path);
        clear_storage_dir(&storage_in_run_dir)?;
    }

    // Reset sequencer database (DA data persists for resync)
    postgres_container.reset_sequencer_db()?;

    // Build resync config with extended stop height
    let resync_versions = build_resync_versions(versions, test_case.extra_blocks_after_resync);

    // For resync, only run soak testing during extra blocks phase
    let resync_soak_config =
        soak_manager_config.and_then(|cfg| cfg.for_resync(test_case.extra_blocks_after_resync));

    run_with_soak(
        manager_binary,
        &resync_versions,
        extra_args,
        run_dir,
        resync_soak_config.as_ref(),
        master_api_url,
        registry,
    )
    .await?;

    info!(name = %test_case.name, "Resync test completed successfully");

    Ok(())
}

/// Run nodes with optional soak testing coordination.
///
/// This coordinates:
/// 1. Spawning all node processes (primary driver)
/// 2. Running soak testing against the master in background (if configured)
/// 3. Waiting for nodes to complete
/// 4. Waiting for soak to finish (with timeout) after nodes complete
///
/// The nodes are the primary driver - we always wait for them to complete.
/// Soak testing runs in the background and should exit shortly after nodes
/// send the shutdown signal.
async fn run_with_soak(
    manager_binary: &Path,
    versions: &NodeVersions,
    extra_args: &[String],
    run_dir: &Path,
    soak_config: Option<&SoakManagerConfig>,
    master_api_url: &str,
    registry: &ProcessRegistry,
) -> Result<(), TestCaseError> {
    if let Some(soak_cfg) = soak_config {
        info!(
            "Starting test with soak testing: {} workers across {} versions",
            soak_cfg.config.num_workers,
            soak_cfg.versions.len()
        );

        // Create shutdown channel for coordination
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Clone data for the spawned tasks
        let manager_binary = manager_binary.to_path_buf();
        let versions = versions.clone();
        let extra_args = extra_args.to_vec();
        let run_dir = run_dir.to_path_buf();
        let soak_cfg = soak_cfg.clone();
        let api_url = master_api_url.to_string();
        let registry = registry.clone(); // Arc clone - shares underlying data

        // Spawn nodes task (primary)
        let nodes_handle = tokio::spawn(async move {
            run_nodes(
                &manager_binary,
                &versions,
                &extra_args,
                &run_dir,
                Some(shutdown_tx),
                &registry,
            )
            .await
        });

        // Spawn soak coordinator task (background)
        let soak_handle =
            tokio::spawn(
                async move { run_soak_coordinator(&soak_cfg, &api_url, shutdown_rx).await },
            );

        // Wait for nodes to complete (primary driver)
        let nodes_result = match nodes_handle.await {
            Ok(Ok(())) => {
                info!("All nodes completed successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                error!(error = %e, "Node(s) failed");
                Err(e)
            }
            Err(join_err) => {
                error!(error = %join_err, "Nodes task panicked");
                Err(TestCaseError::NodeTaskPanic {
                    node_type: "nodes".to_string(),
                    source: join_err,
                })
            }
        };

        // After nodes complete, wait for soak to finish (with timeout for safety)
        // Soak should exit shortly after receiving shutdown signal from nodes
        const SOAK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        match tokio::time::timeout(SOAK_SHUTDOWN_TIMEOUT, soak_handle).await {
            Ok(Ok(Ok(()))) => {
                info!("Soak testing completed");
            }
            Ok(Ok(Err(e))) => {
                // Soak failed - log but don't override nodes error if present
                error!(error = %e, "Soak testing failed");
                if nodes_result.is_ok() {
                    return Err(e);
                }
            }
            Ok(Err(join_err)) => {
                error!(error = %join_err, "Soak task panicked");
                // Don't override nodes error if present
            }
            Err(_) => {
                // Timeout - soak will be killed via kill_on_drop when handle is dropped
                warn!("Soak coordinator did not exit within timeout, forcing shutdown");
            }
        }

        nodes_result
    } else {
        // No soak testing - just run nodes
        info!("Starting test without soak testing");
        run_nodes(
            manager_binary,
            versions,
            extra_args,
            run_dir,
            None,
            registry,
        )
        .await
    }
}

/// Build resync versions with extended stop height.
///
/// Extends the last version's stop height by `extra_blocks` to verify
/// the rollup can continue producing blocks after resyncing.
fn build_resync_versions(versions: &NodeVersions, extra_blocks: u64) -> NodeVersions {
    let mut resync = versions.clone();

    if extra_blocks > 0 {
        // Extend master's last version
        if let Some(last) = resync.master.last_mut() {
            if let Some(stop) = last.stop_height {
                last.stop_height = Some(stop + extra_blocks);
            }
        }

        // Extend each replica's last version
        for replica_versions in &mut resync.replicas {
            if let Some(last) = replica_versions.last_mut() {
                if let Some(stop) = last.stop_height {
                    last.stop_height = Some(stop + extra_blocks);
                }
            }
        }
    }

    resync
}

/// Clear the storage directory while preserving DA data files.
fn clear_storage_dir(storage_path: &Path) -> Result<(), TestCaseError> {
    info!(path = %storage_path.display(), "Clearing and creating fresh storage directory");

    // Remove the entire directory and recreate it
    if storage_path.exists() {
        fs::remove_dir_all(storage_path)
            .map_err(|e| TestCaseError::ClearStorageDir(storage_path.to_path_buf(), e))?;
    }
    fs::create_dir_all(storage_path)
        .map_err(|e| TestCaseError::CreateStorageDir(storage_path.to_path_buf(), e))?;

    Ok(())
}

/// Spawn the mock-da-server process.
///
/// The mock-da-server provides the DA layer for all rollup nodes. It:
/// - Connects to postgres for storage (via `--db`)
/// - Exposes a gRPC endpoint on the specified port (via `--port`)
/// - Runs for the entire test duration (both initial and resync phases)
///
/// The process is registered in the ProcessRegistry for cleanup on Ctrl+C.
async fn spawn_mock_da_server(
    binary_path: &Path,
    da_db_url: &str,
    port: u16,
    registry: &ProcessRegistry,
) -> Result<Child, TestCaseError> {
    info!(
        binary = %binary_path.display(),
        port,
        "Spawning mock-da-server"
    );

    let child = tokio::process::Command::new(binary_path)
        .args([
            "--host",
            "0.0.0.0",
            "--port",
            &port.to_string(),
            "--db",
            da_db_url,
            "--block-time-ms",
            "1000",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true) // Safety net: SIGKILL if SIGTERM from registry didn't work
        .spawn()
        .map_err(TestCaseError::MockDaSpawn)?;

    // Register PID for cleanup on Ctrl+C
    if let Some(pid) = child.id() {
        registry.register(node_config::NodeType::Master, pid); // Use Master as placeholder type
        info!(pid, "mock-da-server started");
    }

    // TODO: poll here instead of waiting
    // NOTE: a too short timeout here causes "Time must be set on initialization." errors from
    // sequencers in multi-node tests. I'm not sure why, longer timeouts are less flaky and 10s
    // seems reliable, though very heavy-handed. TODO: debug in more detail; for now this works.
    tokio::time::sleep(Duration::from_secs(10)).await;

    Ok(child)
}

/// Interpolate config files with connection strings and write to run_dir.
///
/// This performs text substitution of placeholders in each config file,
/// writes the interpolated config to run_dir, and returns updated NodeVersions with
/// the new config paths.
///
/// Placeholders:
/// - `{postgres_connection_string}` - Sequencer database connection string
/// - `{mock_da_connection_string}` - URL to connect to the mock-da-server (e.g., `http://localhost:50051`)
fn interpolate_node_configs(
    versions: NodeVersions,
    run_dir: &Path,
    sequencer_db_url: &str,
    mock_da_url: &str,
) -> Result<NodeVersions, TestCaseError> {
    let mut result = NodeVersions {
        master: Vec::with_capacity(versions.master.len()),
        replicas: Vec::with_capacity(versions.replicas.len()),
    };

    // Interpolate master configs
    for (version_idx, mut version) in versions.master.into_iter().enumerate() {
        let new_path = interpolate_config_file(
            &version.config_path,
            run_dir,
            "master",
            version_idx,
            sequencer_db_url,
            mock_da_url,
        )?;
        version.config_path = new_path;
        result.master.push(version);
    }

    // Interpolate replica configs
    for (replica_idx, replica_versions) in versions.replicas.into_iter().enumerate() {
        let mut interpolated_versions = Vec::with_capacity(replica_versions.len());
        for (version_idx, mut version) in replica_versions.into_iter().enumerate() {
            let new_path = interpolate_config_file(
                &version.config_path,
                run_dir,
                &format!("replica_{replica_idx}"),
                version_idx,
                sequencer_db_url,
                mock_da_url,
            )?;
            version.config_path = new_path;
            interpolated_versions.push(version);
        }
        result.replicas.push(interpolated_versions);
    }

    Ok(result)
}

/// Interpolate a single config file and write to a per-node subdirectory.
///
/// Each node gets its own config directory (e.g., `run_dir/master/`, `run_dir/replica_0/`)
/// to avoid race conditions on files like `evm_pinned_cache.json` that rollups write
/// in the same directory as their config.
///
/// Placeholders replaced:
/// - `{postgres_connection_string}` - Sequencer database connection string
/// - `{mock_da_connection_string}` - URL to connect to mock-da-server (e.g., `http://localhost:50051`)
///
/// Returns the path to the new interpolated config file.
fn interpolate_config_file(
    original_path: &Path,
    run_dir: &Path,
    node_name: &str,
    version_idx: usize,
    sequencer_db_url: &str,
    mock_da_url: &str,
) -> Result<std::path::PathBuf, TestCaseError> {
    // Read original config
    let content = fs::read_to_string(original_path).map_err(|e| TestCaseError::ReadConfig {
        path: original_path.to_path_buf(),
        source: e,
    })?;

    // Perform text substitutions
    let interpolated = content
        .replace("{postgres_connection_string}", sequencer_db_url)
        .replace("{mock_da_url}", mock_da_url);

    // Create per-node config directory to isolate files like evm_pinned_cache.json
    let node_config_dir = run_dir.join(node_name);
    fs::create_dir_all(&node_config_dir).map_err(|e| TestCaseError::WriteConfig {
        path: node_config_dir.clone(),
        source: e,
    })?;

    // Write config with version-only name (node isolation is via directory)
    let new_filename = format!("config_{version_idx}.toml");
    let new_path = node_config_dir.join(&new_filename);

    fs::write(&new_path, &interpolated).map_err(|e| TestCaseError::WriteConfig {
        path: new_path.clone(),
        source: e,
    })?;

    info!(
        original = %original_path.display(),
        interpolated = %new_path.display(),
        "Interpolated config file"
    );

    Ok(new_path)
}
