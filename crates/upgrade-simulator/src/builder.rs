//! Manages rollup repository cloning and binary builds.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use thiserror::Error;
use tracing::info;

/// The default repository URL for the Sovereign SDK rollup starter.
pub const DEFAULT_REPO_URL: &str = "https://github.com/Sovereign-Labs/rollup-starter";

/// Errors that can occur during rollup building.
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

/// Manages the rollup repository and binary builds.
pub struct RollupBuilder {
    /// Root directory for all cached data.
    cache_dir: PathBuf,
    /// Repository URL.
    repo_url: String,
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
            .args([
                "build",
                "--release",
                "--no-default-features",
                "--features",
                "acceptance-testing,mock_da_external,mock_zkvm",
            ])
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

    /// Get the path to the cached mock-da-server binary for a given commit.
    fn mock_da_binary_cache_path(&self, commit: &str) -> PathBuf {
        self.cache_dir
            .join("binaries")
            .join(commit)
            .join("mock-da-server")
    }

    /// Build the mock-da-server binary and cache it.
    fn build_mock_da(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let repo_path = self.repo_path();
        let binary_cache = self.mock_da_binary_cache_path(commit);

        info!(commit, "Building mock-da-server binary");

        let output = Command::new("cargo")
            .current_dir(&repo_path)
            .args([
                "build",
                "--release",
                "--bin",
                "mock-da-server",
                "--no-default-features",
                "--features=mock_da_external,mock_zkvm",
            ])
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
            .join("mock-da-server");

        if !built_binary.exists() {
            return Err(BuilderError::BinaryNotFound(built_binary));
        }

        // Create cache directory and copy binary
        if let Some(parent) = binary_cache.parent() {
            fs::create_dir_all(parent).map_err(BuilderError::CopyBinary)?;
        }
        fs::copy(&built_binary, &binary_cache).map_err(BuilderError::CopyBinary)?;

        info!(path = %binary_cache.display(), "Cached mock-da-server binary");
        Ok(binary_cache)
    }

    /// Get the mock-da-server binary for a specific commit, building if necessary.
    ///
    /// The mock-da-server is built with external DA features:
    /// `--no-default-features --features=mock_da_external,mock_zkvm`
    pub fn get_mock_da_binary(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let binary_cache = self.mock_da_binary_cache_path(commit);

        if binary_cache.exists() {
            info!(commit, path = %binary_cache.display(), "Using cached mock-da-server binary");
            return Ok(binary_cache);
        }

        // Need to build - ensure repo is ready
        self.ensure_repo()?;
        self.checkout(commit)?;
        self.build_mock_da(commit)
    }
}
