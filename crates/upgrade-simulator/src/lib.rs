//! Integration test framework for testing sov-rollup-manager with real Sovereign SDK rollups.
//!
//! This framework manages:
//! - Git repository cloning and checkout of specific commits
//! - Building rollup binaries and caching them by commit hash
//! - Constructing manager configs and running upgrade test cases
//! - Resync testing: clearing state and replaying from DA data

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use serde::Deserialize;
use thiserror::Error;
use tracing::info;

use sov_rollup_manager::{ManagerConfig, RollupVersion};

/// Minimal representation of rollup config for extracting storage path.
#[derive(Debug, Deserialize)]
struct RollupConfig {
    storage: StorageConfig,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    path: PathBuf,
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
}

/// Internal struct for deserializing test_case.toml (without name field).
#[derive(Debug, Deserialize)]
struct TestCaseFile {
    /// Optional repository URL override.
    repo_url: Option<String>,
    versions: Vec<VersionSpec>,
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
        repo_url: file.repo_url.unwrap_or_else(|| DEFAULT_REPO_URL.to_string()),
        versions: file.versions,
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
            .args(["build", "--release"])
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
            .join("rollup");

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
}

/// Run a test case through the rollup manager.
///
/// The config directory is derived as `{test_root}/{test_case.name}`.
/// A temporary run directory is created for rollup state and DA data.
///
/// After a successful initial run, the storage directory is cleared and the test
/// is re-run to verify resync functionality (replaying from DA data).
/// The entire run directory is cleaned up on success.
pub fn run_test_case(
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
        fs::remove_dir_all(&run_dir)
            .map_err(|e| TestCaseError::ClearRunDir(run_dir.clone(), e))?;
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

    // Extract and validate storage paths - all versions must use the same path
    let storage_path = validate_storage_paths(&config_paths)?;
    info!(storage_path = %storage_path.display(), "Validated storage path consistency");

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

    // Run initial test from the run directory
    run_with_working_dir(&run_dir, || {
        // Create storage directory (relative path resolved from run_dir)
        fs::create_dir_all(&storage_path)
            .map_err(|e| TestCaseError::CreateStorageDir(storage_path.clone(), e))?;

        sov_rollup_manager::run(&config, &extra_args).map_err(TestCaseError::Runner)
    })?;

    info!(name = %test_case.name, "Initial run completed successfully");

    // Resync test: clear storage and re-run (DA data persists in run_dir)
    info!(name = %test_case.name, "Starting resync test");

    run_with_working_dir(&run_dir, || {
        clear_storage_dir(&storage_path)?;
        sov_rollup_manager::run(&config, &extra_args).map_err(TestCaseError::Runner)
    })?;

    info!(name = %test_case.name, "Resync test completed successfully");

    // Clean up run directory on success
    fs::remove_dir_all(&run_dir).map_err(|e| TestCaseError::ClearRunDir(run_dir.clone(), e))?;
    info!(name = %test_case.name, "Cleaned up run directory");

    Ok(())
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
    Runner(#[from] sov_rollup_manager::RunnerError),
}

/// Extract the storage path from a rollup config TOML file.
fn extract_storage_path(config_path: &Path) -> Result<PathBuf, TestCaseError> {
    let content = fs::read_to_string(config_path).map_err(|e| TestCaseError::ReadConfig {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    let config: RollupConfig = toml::from_str(&content).map_err(|e| TestCaseError::ParseConfig {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    Ok(config.storage.path)
}
