//! Build and cache rollup-related artifacts across multiple commits.
//!
//! This crate exposes:
//! - A low-level `RollupBuilder` for on-demand binary builds.
//! - A high-level `prepare_artifacts` helper for multi-version specs.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

/// The default repository URL for the Sovereign SDK rollup starter.
pub const DEFAULT_REPO_URL: &str = "https://github.com/Sovereign-Labs/rollup-starter";

const DEFAULT_ROLLUP_CACHE_NAME: &str = "sov-rollup-starter";
const DEFAULT_SOAK_CACHE_NAME: &str = "rollup-starter-soak-test";
const DEFAULT_MOCK_DA_CACHE_NAME: &str = "mock-da-server";

/// Build configuration for a single cargo binary target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildTarget {
    /// Optional package to build (`cargo build -p <package>`).
    #[serde(default)]
    pub package: Option<String>,
    /// Binary name (`cargo build --bin <bin>` and `target/release/<bin>`).
    pub bin: String,
    /// Optional cache file name (defaults to `bin` when omitted).
    #[serde(default)]
    pub cache_name: Option<String>,
    /// Whether to pass `--no-default-features`.
    #[serde(default)]
    pub no_default_features: bool,
    /// Optional cargo features.
    #[serde(default)]
    pub features: Vec<String>,
    /// Additional cargo args appended at the end.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl BuildTarget {
    fn cache_name(&self) -> &str {
        self.cache_name.as_deref().unwrap_or(&self.bin)
    }
}

fn default_rollup_target() -> BuildTarget {
    BuildTarget {
        package: None,
        bin: "rollup".to_string(),
        cache_name: Some(DEFAULT_ROLLUP_CACHE_NAME.to_string()),
        no_default_features: true,
        features: vec![
            "acceptance-testing".to_string(),
            "mock_da_external".to_string(),
            "mock_zkvm".to_string(),
        ],
        extra_args: vec![],
    }
}

fn default_soak_target() -> BuildTarget {
    BuildTarget {
        package: Some("rollup-starter-soak-test".to_string()),
        bin: "rollup-starter-soak-test".to_string(),
        cache_name: Some(DEFAULT_SOAK_CACHE_NAME.to_string()),
        no_default_features: false,
        features: vec![],
        extra_args: vec![],
    }
}

fn default_mock_da_target() -> BuildTarget {
    BuildTarget {
        package: None,
        bin: "mock-da-server".to_string(),
        cache_name: Some(DEFAULT_MOCK_DA_CACHE_NAME.to_string()),
        no_default_features: true,
        features: vec!["mock_da_external".to_string(), "mock_zkvm".to_string()],
        extra_args: vec![],
    }
}

/// Build targets used for artifact preparation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildTargets {
    /// Rollup binary build config.
    #[serde(default = "default_rollup_target")]
    pub rollup: BuildTarget,
    /// Soak binary build config.
    #[serde(default)]
    pub soak: Option<BuildTarget>,
    /// Mock DA binary build config.
    #[serde(default)]
    pub mock_da: Option<BuildTarget>,
}

impl BuildTargets {
    /// Target set used by upgrade-simulator.
    ///
    /// Includes rollup, soak, and mock-da targets.
    pub fn upgrade_simulator_defaults() -> Self {
        Self {
            rollup: default_rollup_target(),
            soak: Some(default_soak_target()),
            mock_da: Some(default_mock_da_target()),
        }
    }
}

impl Default for BuildTargets {
    fn default() -> Self {
        Self {
            rollup: default_rollup_target(),
            soak: None,
            mock_da: None,
        }
    }
}

/// High-level spec for preparing multiple versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSpec {
    /// Optional repository URL override.
    #[serde(default)]
    pub repo_url: Option<String>,
    /// Build targets for artifact preparation.
    #[serde(default)]
    pub targets: BuildTargets,
    /// Versions to prepare.
    pub versions: Vec<VersionBuildSpec>,
}

/// Build spec for one version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionBuildSpec {
    /// Commit hash.
    pub commit: String,
    /// Whether to build soak binary for this version.
    #[serde(default)]
    pub build_soak: bool,
}

/// Runtime request knobs for `prepare_artifacts`.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// Root directory for all cached data.
    pub cache_dir: PathBuf,
    /// Force building soak binaries for all versions.
    pub build_soak_binaries: bool,
    /// Build mock-da binary for the first version commit.
    pub build_mock_da_binary: bool,
}

/// Prepared artifacts for one version.
#[derive(Debug, Clone)]
pub struct PreparedVersionArtifacts {
    pub commit: String,
    pub rollup_binary: PathBuf,
    pub soak_binary: Option<PathBuf>,
}

/// All prepared artifacts for the request.
#[derive(Debug, Clone)]
pub struct PreparedArtifacts {
    pub versions: Vec<PreparedVersionArtifacts>,
    pub mock_da_binary: Option<PathBuf>,
}

/// Errors that can occur during artifact building.
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

    #[error("build target '{0}' is disabled")]
    TargetDisabled(String),
}

/// Manages the rollup repository and binary builds.
pub struct RollupBuilder {
    cache_dir: PathBuf,
    repo_url: String,
    targets: BuildTargets,
}

impl RollupBuilder {
    /// Create a new builder with the default repository URL and rollup-only targets.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            repo_url: DEFAULT_REPO_URL.to_string(),
            targets: BuildTargets::default(),
        }
    }

    /// Create a new builder with a custom repository URL and rollup-only targets.
    pub fn with_repo_url(cache_dir: PathBuf, repo_url: String) -> Self {
        Self {
            repo_url,
            ..Self::new(cache_dir)
        }
    }

    /// Create a new builder with custom repository URL and explicit targets.
    pub fn with_targets(cache_dir: PathBuf, repo_url: String, targets: BuildTargets) -> Self {
        Self {
            cache_dir,
            repo_url,
            targets,
        }
    }

    fn repo_path(&self) -> PathBuf {
        self.cache_dir.join("repo")
    }

    fn binary_cache_path(&self, commit: &str, cache_name: &str) -> PathBuf {
        self.cache_dir
            .join("binaries")
            .join(commit)
            .join(cache_name)
    }

    /// Ensure the repository is cloned and up to date.
    pub fn ensure_repo(&self) -> Result<(), BuilderError> {
        let repo_path = self.repo_path();

        if !repo_path.exists() {
            info!(repo = %self.repo_url, path = %repo_path.display(), "Cloning repository");
            fs::create_dir_all(&self.cache_dir).map_err(BuilderError::CreateCacheDir)?;

            let output = Command::new("git")
                .args([
                    "clone",
                    &self.repo_url,
                    repo_path.to_str().unwrap_or_default(),
                ])
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

    fn build_target(&self, commit: &str, target: &BuildTarget) -> Result<PathBuf, BuilderError> {
        let repo_path = self.repo_path();
        let binary_cache = self.binary_cache_path(commit, target.cache_name());

        info!(commit, bin = %target.bin, "Building binary target");

        let mut args: Vec<String> = vec!["build".to_string(), "--release".to_string()];

        if let Some(package) = &target.package {
            args.push("-p".to_string());
            args.push(package.clone());
        }

        args.push("--bin".to_string());
        args.push(target.bin.clone());

        if target.no_default_features {
            args.push("--no-default-features".to_string());
        }

        if !target.features.is_empty() {
            args.push("--features".to_string());
            args.push(target.features.join(","));
        }

        args.extend(target.extra_args.iter().cloned());

        let output = Command::new("cargo")
            .current_dir(&repo_path)
            .args(&args)
            .output()
            .map_err(|e| BuilderError::CargoBuild(e.to_string()))?;

        if !output.status.success() {
            return Err(BuilderError::CargoBuild(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        let built_binary = repo_path.join("target").join("release").join(&target.bin);
        if !built_binary.exists() {
            return Err(BuilderError::BinaryNotFound(built_binary));
        }

        if let Some(parent) = binary_cache.parent() {
            fs::create_dir_all(parent).map_err(BuilderError::CopyBinary)?;
        }
        fs::copy(&built_binary, &binary_cache).map_err(BuilderError::CopyBinary)?;

        info!(path = %binary_cache.display(), "Cached binary");
        Ok(binary_cache)
    }

    fn get_target_binary(
        &self,
        commit: &str,
        target: &BuildTarget,
    ) -> Result<PathBuf, BuilderError> {
        let binary_cache = self.binary_cache_path(commit, target.cache_name());

        if binary_cache.exists() {
            info!(commit, path = %binary_cache.display(), "Using cached binary");
            return Ok(binary_cache);
        }

        self.ensure_repo()?;
        self.checkout(commit)?;
        self.build_target(commit, target)
    }

    /// Get the rollup binary for a specific commit.
    pub fn get_binary(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        self.get_target_binary(commit, &self.targets.rollup)
    }

    /// Get the soak-test binary for a specific commit.
    pub fn get_soak_binary(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let target = self
            .targets
            .soak
            .as_ref()
            .ok_or_else(|| BuilderError::TargetDisabled("soak".to_string()))?;
        self.get_target_binary(commit, target)
    }

    /// Get the mock-da-server binary for a specific commit.
    pub fn get_mock_da_binary(&self, commit: &str) -> Result<PathBuf, BuilderError> {
        let target = self
            .targets
            .mock_da
            .as_ref()
            .ok_or_else(|| BuilderError::TargetDisabled("mock-da-server".to_string()))?;
        self.get_target_binary(commit, target)
    }
}

/// Prepare artifacts for all versions in the spec.
pub fn prepare_artifacts(
    spec: &BuildSpec,
    request: &BuildRequest,
) -> Result<PreparedArtifacts, BuilderError> {
    let builder = RollupBuilder::with_targets(
        request.cache_dir.clone(),
        spec.repo_url
            .clone()
            .unwrap_or_else(|| DEFAULT_REPO_URL.to_string()),
        spec.targets.clone(),
    );

    let mut versions = Vec::with_capacity(spec.versions.len());
    for version in &spec.versions {
        let rollup_binary = builder.get_binary(&version.commit)?;
        let soak_binary = if request.build_soak_binaries || version.build_soak {
            Some(builder.get_soak_binary(&version.commit)?)
        } else {
            None
        };
        versions.push(PreparedVersionArtifacts {
            commit: version.commit.clone(),
            rollup_binary,
            soak_binary,
        });
    }

    let mock_da_binary = if request.build_mock_da_binary {
        spec.versions
            .first()
            .map(|v| builder.get_mock_da_binary(&v.commit))
            .transpose()?
    } else {
        None
    };

    Ok(PreparedArtifacts {
        versions,
        mock_da_binary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workspace_test_utils::init_local_rollup_repo;

    #[test]
    fn build_target_cache_name_defaults_to_bin() {
        let target = BuildTarget {
            package: None,
            bin: "example-bin".to_string(),
            cache_name: None,
            no_default_features: false,
            features: vec![],
            extra_args: vec![],
        };
        assert_eq!(target.cache_name(), "example-bin");
    }

    #[test]
    fn prepare_artifacts_empty_versions_is_supported() {
        let spec = BuildSpec {
            repo_url: None,
            targets: BuildTargets::default(),
            versions: vec![],
        };
        let req = BuildRequest {
            cache_dir: PathBuf::from("/tmp/nonexistent-cache-dir"),
            build_soak_binaries: false,
            build_mock_da_binary: false,
        };

        let prepared = prepare_artifacts(&spec, &req).expect("prepare should succeed");
        assert!(prepared.versions.is_empty());
        assert!(prepared.mock_da_binary.is_none());
    }

    #[test]
    fn build_targets_default_is_rollup_only() {
        let targets = BuildTargets::default();
        assert_eq!(targets.rollup.bin, "rollup");
        assert!(targets.soak.is_none());
        assert!(targets.mock_da.is_none());
    }

    #[test]
    fn build_targets_upgrade_simulator_defaults_enable_all_targets() {
        let targets = BuildTargets::upgrade_simulator_defaults();
        assert_eq!(targets.rollup.bin, "rollup");
        assert_eq!(
            targets.soak.as_ref().map(|target| target.bin.as_str()),
            Some("rollup-starter-soak-test")
        );
        assert_eq!(
            targets.mock_da.as_ref().map(|target| target.bin.as_str()),
            Some("mock-da-server")
        );
    }

    #[test]
    fn with_targets_disables_soak_when_target_missing() {
        let repo = init_local_rollup_repo();
        let cache_dir = TempDir::new().expect("cache dir");
        let builder = RollupBuilder::with_targets(
            PathBuf::from(cache_dir.path()),
            repo.repo_url(),
            BuildTargets::default(),
        );

        let err = builder
            .get_soak_binary(&repo.commit)
            .expect_err("soak should be disabled");
        assert!(matches!(err, BuilderError::TargetDisabled(name) if name == "soak"));
    }
}
