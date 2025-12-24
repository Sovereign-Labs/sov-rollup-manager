//! Integration test framework for testing sov-rollup-manager with real Sovereign SDK rollups.
//!
//! This framework manages:
//! - Git repository cloning and checkout of specific commits
//! - Building rollup binaries and caching them by commit hash
//! - Constructing manager configs and running upgrade test cases

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use thiserror::Error;
use tracing::info;

use sov_rollup_manager::{ManagerConfig, RollupVersion};

/// The default repository URL for the Sovereign SDK rollup starter.
pub const DEFAULT_REPO_URL: &str = "https://github.com/Sovereign-Labs/rollup-starter";

/// A test case definition specifying versions to upgrade through.
#[derive(Debug, Clone)]
pub struct TestCase {
    /// Human-readable name for this test case (also used as subdirectory name under test_root).
    pub name: String,
    /// The versions to run through, in order.
    pub versions: Vec<VersionSpec>,
}

/// Specification for a single rollup version in a test case.
#[derive(Debug, Clone)]
pub struct VersionSpec {
    /// Git commit hash to build from.
    pub commit: String,
    /// Start height (None for first version).
    pub start_height: Option<u64>,
    /// Stop height (None for last version).
    pub stop_height: Option<u64>,
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
/// The working directory is changed to the test case root for the duration of the run.
pub fn run_test_case(
    builder: &RollupBuilder,
    test_root: &Path,
    test_case: &TestCase,
) -> Result<(), TestCaseError> {
    info!(name = %test_case.name, "Running test case");

    let test_case_root = test_root.join(&test_case.name);
    let test_case_root = test_case_root
        .canonicalize()
        .map_err(|e| TestCaseError::InvalidTestCaseRoot(test_case_root.clone(), e))?;

    // Build all required binaries (paths must be absolute since we change cwd)
    let mut rollup_versions = Vec::new();
    for (i, version_spec) in test_case.versions.iter().enumerate() {
        let binary_path = builder
            .get_binary(&version_spec.commit)
            .map_err(TestCaseError::Build)?;
        let binary_path = binary_path
            .canonicalize()
            .map_err(|e| TestCaseError::InvalidBinaryPath(binary_path.clone(), e))?;

        let config_path = test_case_root.join(format!("v{i}")).join("config.toml");
        if !config_path.exists() {
            return Err(TestCaseError::ConfigNotFound(config_path));
        }

        rollup_versions.push(RollupVersion {
            rollup_binary: binary_path,
            config_path,
            migration_path: None,
            start_height: version_spec.start_height,
            stop_height: version_spec.stop_height,
        });
    }

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

    // Change to test case root directory for the run
    let original_dir = std::env::current_dir().map_err(TestCaseError::WorkingDir)?;
    std::env::set_current_dir(&test_case_root).map_err(TestCaseError::WorkingDir)?;

    let result = sov_rollup_manager::run(&config, &extra_args).map_err(TestCaseError::Runner);

    // Restore original working directory
    let _ = std::env::set_current_dir(original_dir);

    result?;

    info!(name = %test_case.name, "Test case completed successfully");
    Ok(())
}

#[derive(Debug, Error)]
pub enum TestCaseError {
    #[error("failed to build rollup: {0}")]
    Build(#[from] BuilderError),

    #[error("invalid test case root {0}: {1}")]
    InvalidTestCaseRoot(PathBuf, io::Error),

    #[error("invalid binary path {0}: {1}")]
    InvalidBinaryPath(PathBuf, io::Error),

    #[error("config file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("failed to change working directory: {0}")]
    WorkingDir(io::Error),

    #[error("rollup manager failed: {0}")]
    Runner(#[from] sov_rollup_manager::RunnerError),
}
