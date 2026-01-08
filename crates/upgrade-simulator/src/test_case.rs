//! Test case definition and loading.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sov_rollup_manager::RollupVersion;
use thiserror::Error;

use crate::builder::{DEFAULT_REPO_URL, RollupBuilder};
use crate::error::TestCaseError;
use crate::node_config::{NodeConfig, TestNodeLayout};
use crate::node_runner::NodeVersions;
use crate::soak::SoakManagerConfig;

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

impl TestCase {
    /// Build soak manager config if soak testing is enabled.
    ///
    /// Returns `None` if soak testing is not configured.
    /// Builds soak binaries for each version and pairs them with stop heights.
    pub fn build_soak_config(
        &self,
        builder: &RollupBuilder,
    ) -> Result<Option<SoakManagerConfig>, TestCaseError> {
        let soak_config = match &self.soak_testing {
            Some(cfg) => cfg,
            None => return Ok(None),
        };

        let mut versions = Vec::new();
        for version_spec in &self.versions {
            let binary = builder
                .get_soak_binary(&version_spec.commit)
                .map_err(TestCaseError::Build)?;
            let binary = binary
                .canonicalize()
                .map_err(|e| TestCaseError::InvalidBinaryPath(binary.clone(), e))?;
            versions.push((binary, version_spec.stop_height));
        }

        Ok(Some(SoakManagerConfig::new(soak_config.clone(), versions)))
    }

    /// Build RollupVersions for all nodes based on detected layout.
    ///
    /// This builds versions for master and all replicas, using the config paths
    /// detected by `detect_node_layout()`. Single-node tests (config.toml) are
    /// just the degenerate case with empty replicas vec.
    pub fn build_node_versions(
        &self,
        builder: &RollupBuilder,
        layout: &TestNodeLayout,
    ) -> Result<NodeVersions, TestCaseError> {
        // Build versions for master
        let master = self.build_versions_for_node(builder, &layout.master)?;

        // Build versions for each replica
        let replicas: Vec<Vec<RollupVersion>> = layout
            .replicas
            .iter()
            .map(|node| self.build_versions_for_node(builder, node))
            .collect::<Result<_, _>>()?;

        Ok(NodeVersions { master, replicas })
    }

    /// Build RollupVersions for a single node.
    ///
    /// Uses the node's config paths (one per version) to construct RollupVersions.
    fn build_versions_for_node(
        &self,
        builder: &RollupBuilder,
        node: &NodeConfig,
    ) -> Result<Vec<RollupVersion>, TestCaseError> {
        let mut rollup_versions = Vec::new();

        for (i, version_spec) in self.versions.iter().enumerate() {
            // Get the binary for this version
            let binary_path = builder
                .get_binary(&version_spec.commit)
                .map_err(TestCaseError::Build)?;
            let binary_path = binary_path
                .canonicalize()
                .map_err(|e| TestCaseError::InvalidBinaryPath(binary_path.clone(), e))?;

            // Get the config path for this node/version from the layout
            let config_path = &node.config_paths[i];
            let config_path = config_path
                .canonicalize()
                .map_err(|e| TestCaseError::InvalidConfigPath(config_path.clone(), e))?;

            rollup_versions.push(RollupVersion {
                rollup_binary: binary_path,
                config_path,
                migration_path: None,
                start_height: version_spec.start_height,
                stop_height: Some(version_spec.stop_height),
            });
        }

        Ok(rollup_versions)
    }
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
    /// Stop height for this version.
    pub stop_height: u64,
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

/// Errors that can occur when loading a test case.
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
