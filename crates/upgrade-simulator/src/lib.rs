//! Integration test framework for testing sov-rollup-manager with real Sovereign SDK rollups.
//!
//! This framework manages:
//! - Git repository cloning and checkout of specific commits
//! - Building rollup binaries and caching them by commit hash
//! - Constructing manager configs and running upgrade test cases
//! - Resync testing: clearing state and replaying from DA data
//! - Soak testing: generating transaction load during upgrade tests

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{fs, io};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::Deserialize;
use thiserror::Error;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use sov_rollup_manager::{
    HeightCheckMode, ManagerConfig, RollupApiError, RollupVersion, RunnerError, parse_http_port,
    run_with_options,
};

/// Minimal representation of rollup config for extracting storage path.
#[derive(Debug, Deserialize)]
struct RollupConfig {
    storage: StorageConfig,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    path: PathBuf,
}

/// Configuration for soak testing during upgrade simulation.
#[derive(Debug, Clone, Deserialize)]
pub struct SoakTestingConfig {
    /// Number of concurrent soak test workers (default: 5).
    #[serde(default = "default_num_workers")]
    pub num_workers: u32,
    /// RNG salt for reproducibility (default: 0).
    /// Use different salts when restarting to avoid transaction overlap.
    #[serde(default)]
    pub salt: u32,
}

fn default_num_workers() -> u32 {
    5
}

/// The default repository URL for the Sovereign SDK rollup starter.
pub const DEFAULT_REPO_URL: &str = "https://github.com/Sovereign-Labs/rollup-starter";

/// A test case definition specifying versions to upgrade through.
#[derive(Debug, Clone)]
pub struct TestCase {
    /// Human-readable name for this test case (derived from subdirectory name).
    pub name: String,
    /// Repository URL to clone (defaults to DEFAULT_REPO_URL).
    pub repo_url: String,
    /// The versions to run through, in order.
    pub versions: Vec<VersionSpec>,
    /// Extra blocks to run after resync completes on the last version.
    /// This helps verify the rollup can continue producing blocks after resyncing.
    pub extra_blocks_after_resync: u64,
    /// Optional soak testing configuration.
    /// When present, generates transaction load during the upgrade test.
    pub soak_testing: Option<SoakTestingConfig>,
}

/// Internal struct for deserializing test_case.toml (without name field).
#[derive(Debug, Deserialize)]
struct TestCaseFile {
    /// Optional repository URL override.
    repo_url: Option<String>,
    /// Extra blocks to run after resync (defaults to 10).
    #[serde(default = "default_extra_resync_blocks")]
    extra_blocks_after_resync: u64,
    /// Optional soak testing configuration.
    soak_testing: Option<SoakTestingConfig>,
    versions: Vec<VersionSpec>,
}

fn default_extra_resync_blocks() -> u64 {
    10
}

/// Specification for a single rollup version in a test case.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionSpec {
    /// Git commit hash to build from.
    pub commit: String,
    /// Start height (None for first version).
    pub start_height: Option<u64>,
    /// Stop height (None for last version).
    pub stop_height: Option<u64>,
}

/// Load a test case from a directory containing test_case.toml.
///
/// The test case name is derived from the directory name.
pub fn load_test_case(test_case_dir: &Path) -> Result<TestCase, LoadTestCaseError> {
    let name = test_case_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| LoadTestCaseError::InvalidDirName(test_case_dir.to_path_buf()))?
        .to_string();

    let toml_path = test_case_dir.join("test_case.toml");
    let content = fs::read_to_string(&toml_path).map_err(|e| LoadTestCaseError::ReadFile {
        path: toml_path.clone(),
        source: e,
    })?;

    let file: TestCaseFile =
        toml::from_str(&content).map_err(|e| LoadTestCaseError::ParseToml {
            path: toml_path,
            source: e,
        })?;

    Ok(TestCase {
        name,
        repo_url: file
            .repo_url
            .unwrap_or_else(|| DEFAULT_REPO_URL.to_string()),
        versions: file.versions,
        extra_blocks_after_resync: file.extra_blocks_after_resync,
        soak_testing: file.soak_testing,
    })
}

#[derive(Debug, Error)]
pub enum LoadTestCaseError {
    #[error("invalid directory name: {0}")]
    InvalidDirName(PathBuf),

    #[error("failed to read {path}: {source}")]
    ReadFile { path: PathBuf, source: io::Error },

    #[error("failed to parse {path}: {source}")]
    ParseToml {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// Manages the rollup repository and binary builds.
pub struct RollupBuilder {
    /// Root directory for all cached data.
    cache_dir: PathBuf,
    /// Repository URL.
    repo_url: String,
}

#[derive(Debug, Error)]
pub enum BuilderError {
    #[error("failed to create cache directory: {0}")]
    CreateCacheDir(io::Error),

    #[error("git clone failed: {0}")]
    GitClone(String),

    #[error("git fetch failed: {0}")]
    GitFetch(String),

    #[error("git checkout failed for commit {commit}: {message}")]
    GitCheckout { commit: String, message: String },

    #[error("cargo build failed: {0}")]
    CargoBuild(String),

    #[error("failed to copy binary: {0}")]
    CopyBinary(io::Error),

    #[error("binary not found after build at {0}")]
    BinaryNotFound(PathBuf),
}

impl RollupBuilder {
    /// Create a new builder with the default repository URL.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            repo_url: DEFAULT_REPO_URL.to_string(),
        }
    }

    /// Create a new builder with a custom repository URL.
    pub fn with_repo_url(cache_dir: PathBuf, repo_url: String) -> Self {
        Self {
            cache_dir,
            repo_url,
        }
    }

    /// Get the path to the cached repository clone.
    fn repo_path(&self) -> PathBuf {
        self.cache_dir.join("repo")
    }

    /// Get the path to the cached binary for a given commit.
    fn binary_cache_path(&self, commit: &str) -> PathBuf {
        self.cache_dir
            .join("binaries")
            .join(commit)
            .join("sov-rollup-starter")
    }

    /// Ensure the repository is cloned and up to date.
    pub fn ensure_repo(&self) -> Result<(), BuilderError> {
        let repo_path = self.repo_path();

        if !repo_path.exists() {
            info!(repo = %self.repo_url, path = %repo_path.display(), "Cloning repository");
            fs::create_dir_all(&self.cache_dir).map_err(BuilderError::CreateCacheDir)?;

            let output = Command::new("git")
                .args(["clone", &self.repo_url, repo_path.to_str().unwrap()])
                .output()
                .map_err(|e| BuilderError::GitClone(e.to_string()))?;

            if !output.status.success() {
                return Err(BuilderError::GitClone(
                    String::from_utf8_lossy(&output.stderr).to_string(),
                ));
            }
        } else {
            info!(path = %repo_path.display(), "Fetching latest from repository");
            let output = Command::new("git")
                .current_dir(&repo_path)
                .args(["fetch", "--all"])
                .output()
                .map_err(|e| BuilderError::GitFetch(e.to_string()))?;

            if !output.status.success() {
                return Err(BuilderError::GitFetch(
                    String::from_utf8_lossy(&output.stderr).to_string(),
                ));
            }
        }

        Ok(())
    }

    /// Checkout a specific commit in the repository.
    fn checkout(&self, commit: &str) -> Result<(), BuilderError> {
        let repo_path = self.repo_path();
        info!(commit, "Checking out commit");

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["checkout", commit])
            .output()
            .map_err(|e| BuilderError::GitCheckout {
                commit: commit.to_string(),
                message: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(BuilderError::GitCheckout {
                commit: commit.to_string(),
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Build the rollup and cache the binary.
    fn build(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let repo_path = self.repo_path();
        let binary_cache = self.binary_cache_path(commit);

        info!(commit, "Building rollup");

        let output = Command::new("cargo")
            .current_dir(&repo_path)
            .args(["build", "--release", "--features", "acceptance-testing"])
            .output()
            .map_err(|e| BuilderError::CargoBuild(e.to_string()))?;

        if !output.status.success() {
            return Err(BuilderError::CargoBuild(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // Find and copy the binary
        let built_binary = repo_path.join("target").join("release").join("rollup");

        if !built_binary.exists() {
            return Err(BuilderError::BinaryNotFound(built_binary));
        }

        // Create cache directory and copy binary
        if let Some(parent) = binary_cache.parent() {
            fs::create_dir_all(parent).map_err(BuilderError::CopyBinary)?;
        }
        fs::copy(&built_binary, &binary_cache).map_err(BuilderError::CopyBinary)?;

        info!(path = %binary_cache.display(), "Cached binary");
        Ok(binary_cache)
    }

    /// Get the binary for a specific commit, building if necessary.
    pub fn get_binary(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let binary_cache = self.binary_cache_path(commit);

        if binary_cache.exists() {
            info!(commit, path = %binary_cache.display(), "Using cached binary");
            return Ok(binary_cache);
        }

        // Need to build
        self.ensure_repo()?;
        self.checkout(commit)?;
        self.build(commit)
    }

    /// Get the path to the cached soak-test binary for a given commit.
    fn soak_binary_cache_path(&self, commit: &str) -> PathBuf {
        self.cache_dir
            .join("binaries")
            .join(commit)
            .join("rollup-starter-soak-test")
    }

    /// Build the soak-test binary and cache it.
    fn build_soak(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let repo_path = self.repo_path();
        let binary_cache = self.soak_binary_cache_path(commit);

        info!(commit, "Building soak-test binary");

        let output = Command::new("cargo")
            .current_dir(&repo_path)
            .args(["build", "--release", "-p", "rollup-starter-soak-test"])
            .output()
            .map_err(|e| BuilderError::CargoBuild(e.to_string()))?;

        if !output.status.success() {
            return Err(BuilderError::CargoBuild(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // Find and copy the binary
        let built_binary = repo_path
            .join("target")
            .join("release")
            .join("rollup-starter-soak-test");

        if !built_binary.exists() {
            return Err(BuilderError::BinaryNotFound(built_binary));
        }

        // Create cache directory and copy binary
        if let Some(parent) = binary_cache.parent() {
            fs::create_dir_all(parent).map_err(BuilderError::CopyBinary)?;
        }
        fs::copy(&built_binary, &binary_cache).map_err(BuilderError::CopyBinary)?;

        info!(path = %binary_cache.display(), "Cached soak-test binary");
        Ok(binary_cache)
    }

    /// Get the soak-test binary for a specific commit, building if necessary.
    pub fn get_soak_binary(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let binary_cache = self.soak_binary_cache_path(commit);

        if binary_cache.exists() {
            info!(commit, path = %binary_cache.display(), "Using cached soak-test binary");
            return Ok(binary_cache);
        }

        // Need to build - ensure repo is ready
        self.ensure_repo()?;
        self.checkout(commit)?;
        self.build_soak(commit)
    }
}

// =============================================================================
// Async HTTP helpers for soak testing coordination
// =============================================================================

/// Wait for the rollup sequencer to be ready to accept transactions.
///
/// Polls `/sequencer/ready` endpoint until it returns a success response.
/// This is an unbounded loop intended to be raced against manager completion
/// via `tokio::select!`.
async fn wait_for_sequencer_ready(api_url: &str) -> Result<(), TestCaseError> {
    let client = reqwest::Client::new();
    let url = format!("{}/sequencer/ready", api_url);

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
async fn wait_for_height(port: u16, target: u64) -> Result<(), TestCaseError> {
    let client = reqwest::Client::new();
    let url = format!(
        "http://localhost:{}/modules/chain-state/state/current-heights/",
        port
    );

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
fn start_soak_process(
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
/// Sends SIGTERM and waits for graceful shutdown (up to 10 seconds),
/// then falls back to SIGKILL if necessary.
///
/// Returns an error if the process had already exited with a failure code
/// before we tried to stop it (indicating an unexpected crash).
async fn stop_soak_process(mut child: Child) -> Result<(), TestCaseError> {
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
async fn stop_soak_process_if_running(
    soak_process: &mut Option<Child>,
) -> Result<(), TestCaseError> {
    if let Some(child) = soak_process.take() {
        stop_soak_process(child).await?;
    }
    Ok(())
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

    // Validate that the last version has a stop_height - test cases must be bounded
    if let Some(last_version) = test_case.versions.last() {
        if last_version.stop_height.is_none() {
            return Err(TestCaseError::LastVersionMissingStopHeight);
        }
    }

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

    // Build all required binaries and collect config paths
    let mut rollup_versions = Vec::new();
    let mut config_paths = Vec::new();

    for (i, version_spec) in test_case.versions.iter().enumerate() {
        let binary_path = builder
            .get_binary(&version_spec.commit)
            .map_err(TestCaseError::Build)?;
        let binary_path = binary_path
            .canonicalize()
            .map_err(|e| TestCaseError::InvalidBinaryPath(binary_path.clone(), e))?;

        let config_path = test_case_root.join(format!("v{i}")).join("config.toml");
        if !config_path.exists() {
            return Err(TestCaseError::ConfigNotFound(config_path.clone()));
        }
        let config_path = config_path
            .canonicalize()
            .map_err(|e| TestCaseError::InvalidConfigPath(config_path.clone(), e))?;

        config_paths.push(config_path.clone());

        rollup_versions.push(RollupVersion {
            rollup_binary: binary_path,
            config_path,
            migration_path: None,
            start_height: version_spec.start_height,
            stop_height: version_spec.stop_height,
        });
    }

    // Build soak-test binaries and pair with stop heights if soak testing is enabled
    // Each entry is (binary_path, stop_height)
    let soak_versions: Option<Vec<(PathBuf, u64)>> = if test_case.soak_testing.is_some() {
        let mut versions = Vec::new();
        for version_spec in &test_case.versions {
            let binary = builder
                .get_soak_binary(&version_spec.commit)
                .map_err(TestCaseError::Build)?;
            let binary = binary
                .canonicalize()
                .map_err(|e| TestCaseError::InvalidBinaryPath(binary.clone(), e))?;
            // All versions must have stop_height (validated earlier)
            let stop_height = version_spec.stop_height.expect("stop_height validated");
            versions.push((binary, stop_height));
        }
        Some(versions)
    } else {
        None
    };

    // Extract and validate storage paths - all versions must use the same path
    let storage_path = validate_storage_paths(&config_paths)?;
    info!(storage_path = %storage_path.display(), "Validated storage path consistency");

    // Extract HTTP port from the first config (for soak testing coordination)
    let http_port = parse_http_port(&config_paths[0])?;
    let api_url = format!("http://localhost:{}", http_port);

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
        test_case.soak_testing.as_ref(),
        soak_versions.as_deref(),
        &api_url,
        http_port,
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
    let resync_soak_versions: Option<Vec<(PathBuf, u64)>> =
        if test_case.extra_blocks_after_resync > 0 {
            soak_versions.as_ref().map(|versions| {
                let (last_binary, _) = versions.last().expect("has versions");
                let extended_stop = resync_config
                    .versions
                    .last()
                    .and_then(|v| v.stop_height)
                    .expect("resync config has stop height");
                vec![(last_binary.clone(), extended_stop)]
            })
        } else {
            None
        };

    let resync_soak_config = if resync_soak_versions.is_some() {
        test_case.soak_testing.clone().map(|mut c| {
            let salt_increment = c.num_workers * test_case.versions.len() as u32;
            c.salt += salt_increment;
            c
        })
    } else {
        None
    };

    run_manager_with_soak(
        &resync_config,
        &extra_args,
        HeightCheckMode::LenientIntermediateOnly,
        &run_dir,
        resync_soak_config.as_ref(),
        resync_soak_versions.as_deref(),
        &api_url,
        http_port,
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
/// If `soak_versions` is provided, soak testing will run alongside the manager.
/// Each entry is a `(binary_path, stop_height)` pair.
async fn run_manager_with_soak(
    config: &ManagerConfig,
    extra_args: &[String],
    height_check_mode: HeightCheckMode,
    run_dir: &Path,
    soak_config: Option<&SoakTestingConfig>,
    soak_versions: Option<&[(PathBuf, u64)]>,
    api_url: &str,
    http_port: u16,
) -> Result<(), TestCaseError> {
    // Change to run directory for the manager
    let original_dir = std::env::current_dir().map_err(TestCaseError::WorkingDir)?;
    std::env::set_current_dir(run_dir).map_err(TestCaseError::WorkingDir)?;

    let result = if let (Some(soak_cfg), Some(versions)) = (soak_config, soak_versions) {
        run_manager_with_soak_inner(
            config,
            extra_args,
            height_check_mode,
            soak_cfg,
            versions,
            api_url,
            http_port,
        )
        .await
    } else {
        // No soak testing - just run the manager
        let config = config.clone();
        let extra_args = extra_args.to_vec();
        tokio::task::spawn_blocking(move || {
            run_with_options(&config, &extra_args, height_check_mode)
        })
        .await
        .map_err(TestCaseError::ManagerTaskPanic)?
        .map_err(TestCaseError::Runner)
    };

    // Restore original working directory
    let _ = std::env::set_current_dir(original_dir);

    result
}

/// Inner implementation of soak testing coordination.
///
/// Runs the manager in a background task and coordinates soak-test workers
/// alongside it, detecting version transitions via height polling.
///
/// Each entry in `soak_versions` is a `(binary_path, stop_height)` pair.
async fn run_manager_with_soak_inner(
    config: &ManagerConfig,
    extra_args: &[String],
    height_check_mode: HeightCheckMode,
    soak_config: &SoakTestingConfig,
    soak_versions: &[(PathBuf, u64)],
    api_url: &str,
    http_port: u16,
) -> Result<(), TestCaseError> {
    info!(
        "Starting soak testing with {} workers across {} versions",
        soak_config.num_workers,
        soak_versions.len()
    );

    // Spawn manager in background
    let config = config.clone();
    let extra_args = extra_args.to_vec();
    let mut manager_handle = tokio::task::spawn_blocking(move || {
        run_with_options(&config, &extra_args, height_check_mode)
    });

    // Run soak coordinator
    run_soak_coordinator(
        soak_versions,
        soak_config,
        api_url,
        http_port,
        &mut manager_handle,
    )
    .await
}

/// Coordinate soak testing alongside the rollup manager.
///
/// Monitors height to detect version transitions, stopping and restarting
/// soak workers at each transition point.
///
/// Each entry in `soak_versions` is a `(binary_path, stop_height)` pair.
/// The number of entries must match the number of versions being run.
async fn run_soak_coordinator(
    soak_versions: &[(PathBuf, u64)],
    config: &SoakTestingConfig,
    api_url: &str,
    http_port: u16,
    manager_handle: &mut JoinHandle<Result<(), RunnerError>>,
) -> Result<(), TestCaseError> {
    let mut soak_process: Option<Child> = None;
    let mut current_salt = config.salt;

    for (idx, (binary, stop_height)) in soak_versions.iter().enumerate() {
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
            config.num_workers,
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
            _ = wait_for_height(http_port, *stop_height) => {
                info!(
                    version = idx,
                    stop_height,
                    "Reached stop height, transitioning to next version"
                );
            }
        }

        // Stop soak workers before version transition
        stop_soak_process_if_running(&mut soak_process).await?;
        current_salt += config.num_workers; // Avoid tx overlap with new salt
    }

    // Wait for manager to finish
    let result = manager_handle.await;
    stop_soak_process_if_running(&mut soak_process).await?;

    result
        .map_err(TestCaseError::ManagerTaskPanic)?
        .map_err(TestCaseError::Runner)
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
fn validate_storage_paths(config_paths: &[PathBuf]) -> Result<PathBuf, TestCaseError> {
    let mut first_path: Option<PathBuf> = None;

    for (i, config_path) in config_paths.iter().enumerate() {
        let storage_path = extract_storage_path(config_path)?;

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

/// Run a closure with a temporary working directory change.
#[allow(dead_code)]
fn run_with_working_dir<F, T>(dir: &Path, f: F) -> Result<T, TestCaseError>
where
    F: FnOnce() -> Result<T, TestCaseError>,
{
    let original_dir = std::env::current_dir().map_err(TestCaseError::WorkingDir)?;
    std::env::set_current_dir(dir).map_err(TestCaseError::WorkingDir)?;

    let result = f();

    // Restore original working directory
    let _ = std::env::set_current_dir(original_dir);

    result
}

#[derive(Debug, Error)]
pub enum TestCaseError {
    #[error("last version must have a stop_height defined (test cases must be bounded)")]
    LastVersionMissingStopHeight,

    #[error("failed to build rollup: {0}")]
    Build(#[from] BuilderError),

    #[error("invalid test case root {0}: {1}")]
    InvalidTestCaseRoot(PathBuf, io::Error),

    #[error("invalid binary path {0}: {1}")]
    InvalidBinaryPath(PathBuf, io::Error),

    #[error("invalid config path {0}: {1}")]
    InvalidConfigPath(PathBuf, io::Error),

    #[error("config file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("failed to read config {path}: {source}")]
    ReadConfig { path: PathBuf, source: io::Error },

    #[error("failed to parse config {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("storage path mismatch: v0 uses {first}, but v{version} uses {other}")]
    StoragePathMismatch {
        first: PathBuf,
        version: usize,
        other: PathBuf,
    },

    #[error("failed to create run directory {0}: {1}")]
    CreateRunDir(PathBuf, io::Error),

    #[error("failed to clear run directory {0}: {1}")]
    ClearRunDir(PathBuf, io::Error),

    #[error("failed to create storage directory {0}: {1}")]
    CreateStorageDir(PathBuf, io::Error),

    #[error("failed to clear storage directory {0}: {1}")]
    ClearStorageDir(PathBuf, io::Error),

    #[error("failed to change working directory: {0}")]
    WorkingDir(io::Error),

    #[error("rollup manager failed: {0}")]
    Runner(sov_rollup_manager::RunnerError),

    #[error("failed to parse HTTP port from config: {0}")]
    HttpPortParse(#[from] RollupApiError),

    #[error("failed to start soak-test process: {0}")]
    SoakTestStartFailed(io::Error),

    #[error("soak-test process failed with exit code {exit_code}")]
    SoakTestFailed { exit_code: i32 },

    #[error("manager task panicked: {0}")]
    ManagerTaskPanic(tokio::task::JoinError),
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
