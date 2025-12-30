//! Integration test framework for testing sov-rollup-manager with real Sovereign SDK rollups.
//!
//! This framework manages:
//! - Git repository cloning and checkout of specific commits
//! - Building rollup binaries and caching them by commit hash
//! - Constructing manager configs and running upgrade test cases
//! - Resync testing: clearing state and replaying from DA data
//! - Soak testing: generating transaction load during upgrade tests

mod builder;
mod error;
mod soak;
mod test_case;

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sov_rollup_manager::{HeightCheckMode, ManagerConfig, parse_http_port, run_with_options};
use tracing::info;

pub use builder::{BuilderError, DEFAULT_REPO_URL, RollupBuilder};
pub use error::TestCaseError;
pub use test_case::{LoadTestCaseError, SoakTestingConfig, TestCase, VersionSpec, load_test_case};

use soak::{SoakManagerConfig, run_soak_coordinator};

/// Minimal representation of rollup config for extracting storage path.
#[derive(Debug, Deserialize)]
struct RollupConfig {
    storage: StorageConfig,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    path: PathBuf,
}

// =============================================================================
// Test case execution
// =============================================================================

/// Run a test case through the rollup manager.
///
/// The config directory is derived as `{test_root}/{test_case.name}`.
/// A temporary run directory is created for rollup state and DA data.
///
/// After a successful initial run, the storage directory is cleared and the test
/// is re-run to verify resync functionality (replaying from DA data).
/// The entire run directory is cleaned up on success.
///
/// If soak testing is configured, transaction load is generated alongside
/// the rollup during the test.
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

    // Create temporary run directory for rollup state and DA data
    let run_dir = test_case_root.join("run-dir");
    if run_dir.exists() {
        fs::remove_dir_all(&run_dir).map_err(|e| TestCaseError::ClearRunDir(run_dir.clone(), e))?;
    }
    fs::create_dir_all(&run_dir).map_err(|e| TestCaseError::CreateRunDir(run_dir.clone(), e))?;
    info!(run_dir = %run_dir.display(), "Created run directory");

    // Build all required binaries
    let rollup_versions = test_case.build_rollup_versions(&builder, &test_case_root)?;

    // Build soak manager config if soak testing is enabled
    let soak_manager_config = test_case.build_soak_config(&builder)?;

    // Extract and validate storage paths - all versions must use the same path
    let config_paths: Vec<_> = rollup_versions.iter().map(|v| &v.config_path).collect();
    let storage_path = validate_storage_paths(&config_paths)?;
    info!(storage_path = %storage_path.display(), "Validated storage path consistency");

    // Extract HTTP port from the first config (for soak testing coordination)
    let http_port = parse_http_port(config_paths[0])?;
    let api_url = format!("http://localhost:{http_port}");

    // Construct the manager config
    let config = ManagerConfig {
        versions: rollup_versions,
    };

    // Genesis file is expected at {test_case_root}/genesis.json
    let genesis_path = test_case_root.join("genesis.json");
    let extra_args = vec![
        "--genesis-path".to_string(),
        genesis_path.to_string_lossy().into_owned(),
    ];

    // Create storage directory (relative path resolved from run_dir)
    let storage_in_run_dir = run_dir.join(&storage_path);
    fs::create_dir_all(&storage_in_run_dir)
        .map_err(|e| TestCaseError::CreateStorageDir(storage_in_run_dir.clone(), e))?;

    // Run initial test
    run_manager_with_soak(
        &config,
        &extra_args,
        HeightCheckMode::Strict,
        &run_dir,
        soak_manager_config.as_ref(),
        &api_url,
    )
    .await?;

    info!(name = %test_case.name, "Initial run completed successfully");

    // Resync test: clear storage and re-run (DA data persists in run_dir)
    // Use lenient height checking since fast resyncs may exit before we can poll the final height.
    // Also extend the last version's stop height by extra_resync_blocks to verify continued operation.
    info!(
        name = %test_case.name,
        extra_blocks = test_case.extra_blocks_after_resync,
        "Starting resync test"
    );

    let resync_config = build_resync_config(&config, test_case.extra_blocks_after_resync);

    // Clear storage for resync
    clear_storage_dir(&storage_in_run_dir)?;

    // For resync, only run soak testing during extra blocks phase.
    // During resync the rollup replays existing transactions and won't accept new ones
    // until it catches up, so soak only runs during the extra blocks at the end.
    let resync_soak_config = soak_manager_config
        .as_ref()
        .and_then(|cfg| cfg.for_resync(test_case.extra_blocks_after_resync));

    run_manager_with_soak(
        &resync_config,
        &extra_args,
        HeightCheckMode::LenientIntermediateOnly,
        &run_dir,
        resync_soak_config.as_ref(),
        &api_url,
    )
    .await?;

    info!(name = %test_case.name, "Resync test completed successfully");

    // Clean up run directory on success
    fs::remove_dir_all(&run_dir).map_err(|e| TestCaseError::ClearRunDir(run_dir.clone(), e))?;
    info!(name = %test_case.name, "Cleaned up run directory");

    Ok(())
}

/// Run the rollup manager with optional soak testing coordination.
///
/// If `soak_config` is provided, soak testing will run alongside the manager.
async fn run_manager_with_soak(
    config: &ManagerConfig,
    extra_args: &[String],
    height_check_mode: HeightCheckMode,
    run_dir: &Path,
    soak_config: Option<&SoakManagerConfig>,
    api_url: &str,
) -> Result<(), TestCaseError> {
    // Change to run directory for the manager
    let original_dir = std::env::current_dir().map_err(TestCaseError::WorkingDir)?;
    std::env::set_current_dir(run_dir).map_err(TestCaseError::WorkingDir)?;

    let result = match soak_config {
        Some(soak_cfg) => {
            info!(
                "Starting soak testing with {} workers across {} versions",
                soak_cfg.config.num_workers,
                soak_cfg.versions.len()
            );

            // Spawn manager in background
            let config = config.clone();
            let extra_args = extra_args.to_vec();
            let mut manager_handle = tokio::task::spawn_blocking(move || {
                run_with_options(&config, &extra_args, height_check_mode)
            });

            run_soak_coordinator(soak_cfg, api_url, &mut manager_handle).await
        }
        None => {
            // No soak testing - just run the manager
            let config = config.clone();
            let extra_args = extra_args.to_vec();
            tokio::task::spawn_blocking(move || {
                run_with_options(&config, &extra_args, height_check_mode)
            })
            .await
            .map_err(TestCaseError::ManagerTaskPanic)?
            .map_err(TestCaseError::Runner)
        }
    };

    // Restore original working directory
    let _ = std::env::set_current_dir(original_dir);

    result
}

/// Build a modified config for resync testing.
///
/// Extends the last version's stop height by `extra_blocks` to verify
/// the rollup can continue producing blocks after resyncing.
fn build_resync_config(config: &ManagerConfig, extra_blocks: u64) -> ManagerConfig {
    let mut resync_config = config.clone();

    if extra_blocks > 0 {
        if let Some(last_version) = resync_config.versions.last_mut() {
            if let Some(stop) = last_version.stop_height {
                last_version.stop_height = Some(stop + extra_blocks);
            }
        }
    }

    resync_config
}

/// Validate that all config files use the same storage path.
fn validate_storage_paths<P: AsRef<Path>>(config_paths: &[P]) -> Result<PathBuf, TestCaseError> {
    let mut first_path: Option<PathBuf> = None;

    for (i, config_path) in config_paths.iter().enumerate() {
        let storage_path = extract_storage_path(config_path.as_ref())?;

        match &first_path {
            None => first_path = Some(storage_path),
            Some(first) if first != &storage_path => {
                return Err(TestCaseError::StoragePathMismatch {
                    first: first.clone(),
                    version: i,
                    other: storage_path,
                });
            }
            Some(_) => {}
        }
    }

    first_path.ok_or_else(|| TestCaseError::ConfigNotFound(PathBuf::from("no configs provided")))
}

/// Clear the storage directory while preserving DA data files.
fn clear_storage_dir(storage_path: &Path) -> Result<(), TestCaseError> {
    info!(path = %storage_path.display(), "Clearing storage directory for resync");

    // Remove the entire directory and recreate it
    if storage_path.exists() {
        fs::remove_dir_all(storage_path)
            .map_err(|e| TestCaseError::ClearStorageDir(storage_path.to_path_buf(), e))?;
    }
    fs::create_dir_all(storage_path)
        .map_err(|e| TestCaseError::CreateStorageDir(storage_path.to_path_buf(), e))?;

    Ok(())
}

/// Extract the storage path from a rollup config TOML file.
fn extract_storage_path(config_path: &Path) -> Result<PathBuf, TestCaseError> {
    let content = fs::read_to_string(config_path).map_err(|e| TestCaseError::ReadConfig {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    let config: RollupConfig =
        toml::from_str(&content).map_err(|e| TestCaseError::ParseConfig {
            path: config_path.to_path_buf(),
            source: e,
        })?;

    Ok(config.storage.path)
}
