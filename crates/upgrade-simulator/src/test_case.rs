//! Test case definition and loading.
//!
//! A test case specifies versions to upgrade through and the node layout
//! (master + optional replicas). Layout detection happens at load time.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sov_rollup_manager::RollupVersion;
use tracing::info;

use crate::builder::{DEFAULT_REPO_URL, RollupBuilder};
use crate::error::TestCaseError;
use crate::node_runner::NodeVersions;
use crate::soak::SoakManagerConfig;

/// Type of node in a test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeType {
    /// Master node (or single node in non-replication mode).
    Master,
    /// Replica node with its index (0-indexed).
    Replica(usize),
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeType::Master => write!(f, "master"),
            NodeType::Replica(i) => write!(f, "replica_{i}"),
        }
    }
}

/// Configuration for a single node across all versions.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Type of this node.
    pub node_type: NodeType,
    /// Config file paths for each version (indexed by version number).
    pub config_paths: Vec<PathBuf>,
    /// HTTP port for the rollup API (must be same across all versions).
    pub http_port: u16,
    /// Storage path (must be same across all versions).
    pub storage_path: PathBuf,
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
    /// Node configurations. nodes[0] is always master, nodes[1..] are replicas.
    /// Single-node tests have exactly one node.
    pub nodes: Vec<NodeInfo>,
}

impl TestCase {
    /// Get the number of replicas (0 for single-node tests).
    pub fn num_replicas(&self) -> usize {
        self.nodes.len().saturating_sub(1)
    }

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

    /// Build RollupVersions for all nodes.
    ///
    /// Returns a flat structure where nodes[0] is master, nodes[1..] are replicas.
    pub fn build_node_versions(
        &self,
        builder: &RollupBuilder,
    ) -> Result<NodeVersions, TestCaseError> {
        let mut nodes = Vec::with_capacity(self.nodes.len());

        for node in &self.nodes {
            let node_versions = self.build_versions_for_node(builder, node)?;
            nodes.push(node_versions);
        }

        Ok(NodeVersions { nodes })
    }

    /// Build RollupVersions for a single node.
    ///
    /// Uses the node's config paths (one per version) to construct RollupVersions.
    fn build_versions_for_node(
        &self,
        builder: &RollupBuilder,
        node: &NodeInfo,
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

            // Get the config path for this node/version
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
/// Node layout is detected from config files in version subdirectories.
pub fn load_test_case(test_case_dir: &Path) -> Result<TestCase, TestCaseError> {
    let name = test_case_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| TestCaseError::InvalidDirName(test_case_dir.to_path_buf()))?
        .to_string();

    let toml_path = test_case_dir.join("test_case.toml");
    let content = fs::read_to_string(&toml_path).map_err(|e| TestCaseError::ReadTestCase {
        path: toml_path.clone(),
        source: e,
    })?;

    let file: TestCaseFile =
        toml::from_str(&content).map_err(|e| TestCaseError::ParseTestCase {
            path: toml_path,
            source: e,
        })?;

    if file.versions.is_empty() {
        return Err(TestCaseError::NoVersions);
    }

    // Detect node layout from config files
    let nodes = detect_nodes(test_case_dir, file.versions.len())?;

    // Validate no port or storage path conflicts
    validate_nodes(&nodes)?;

    info!(
        num_nodes = nodes.len(),
        num_replicas = nodes.len().saturating_sub(1),
        "Detected node layout"
    );

    Ok(TestCase {
        name,
        repo_url: file
            .repo_url
            .unwrap_or_else(|| DEFAULT_REPO_URL.to_string()),
        versions: file.versions,
        extra_blocks_after_resync: file.extra_blocks_after_resync,
        soak_testing: file.soak_testing,
        nodes,
    })
}

/// Minimal representation of rollup config for extracting needed fields.
#[derive(Debug, Deserialize)]
struct RollupConfig {
    storage: StorageConfig,
    runner: RunnerConfig,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RunnerConfig {
    http_config: HttpConfig,
}

#[derive(Debug, Deserialize)]
struct HttpConfig {
    bind_port: u16,
}

/// Detect node layout from a test case directory.
///
/// Examines config files in each version directory to determine:
/// - Whether this is a single-node test (config.toml) or replication test (config_master.toml + config_replica_N.toml)
/// - The number of replicas
/// - The HTTP port and storage path for each node
fn detect_nodes(test_case_dir: &Path, num_versions: usize) -> Result<Vec<NodeInfo>, TestCaseError> {
    // Detect mode and replica count from v0
    let v0_dir = test_case_dir.join("v0");
    let (is_replication_mode, num_replicas) = detect_mode_and_replicas(&v0_dir)?;

    info!(is_replication_mode, num_replicas, "Detected config mode");

    // Collect config paths for each node across all versions
    let num_nodes = 1 + num_replicas; // master + replicas
    let mut node_paths: Vec<Vec<PathBuf>> = (0..num_nodes)
        .map(|_| Vec::with_capacity(num_versions))
        .collect();

    for version_idx in 0..num_versions {
        let version_dir = test_case_dir.join(format!("v{version_idx}"));

        // Get master config path
        let master_config = if is_replication_mode {
            version_dir.join("config_master.toml")
        } else {
            version_dir.join("config.toml")
        };

        if !master_config.exists() {
            return Err(TestCaseError::ConfigNotFound(master_config));
        }
        node_paths[0].push(master_config);

        // Get replica config paths
        for replica_idx in 0..num_replicas {
            let replica_config = version_dir.join(format!("config_replica_{replica_idx}.toml"));
            if !replica_config.exists() {
                return Err(TestCaseError::ConfigNotFound(replica_config));
            }
            node_paths[1 + replica_idx].push(replica_config);
        }

        // Verify no extra replicas in this version
        let actual_replicas = count_replica_configs(&version_dir)?;
        if actual_replicas != num_replicas {
            return Err(TestCaseError::ReplicaCountMismatch {
                version: version_idx,
                count: actual_replicas,
                expected: num_replicas,
            });
        }
    }

    // Build NodeInfo for each node
    let mut nodes = Vec::with_capacity(num_nodes);

    // Master node
    nodes.push(build_node_info(NodeType::Master, node_paths.remove(0))?);

    // Replica nodes
    for (idx, paths) in node_paths.into_iter().enumerate() {
        nodes.push(build_node_info(NodeType::Replica(idx), paths)?);
    }

    Ok(nodes)
}

/// Detect whether a version directory uses replication mode and count replicas.
fn detect_mode_and_replicas(version_dir: &Path) -> Result<(bool, usize), TestCaseError> {
    let master_config = version_dir.join("config_master.toml");
    let simple_config = version_dir.join("config.toml");

    let is_replication_mode = if master_config.exists() {
        true
    } else if simple_config.exists() {
        false
    } else {
        return Err(TestCaseError::ConfigNotFound(simple_config));
    };

    let num_replicas = count_replica_configs(version_dir)?;

    // If we found replicas but no config_master.toml, that's an error
    if num_replicas > 0 && !is_replication_mode {
        return Err(TestCaseError::ConfigModeMismatch {
            version: 0,
            mode: "simple (config.toml)".to_string(),
            expected_mode: "replication (config_master.toml required with replicas)".to_string(),
        });
    }

    Ok((is_replication_mode, num_replicas))
}

/// Count the number of replica config files in a version directory.
///
/// Replicas must be numbered contiguously starting from 0.
fn count_replica_configs(version_dir: &Path) -> Result<usize, TestCaseError> {
    let mut count = 0;
    loop {
        let replica_config = version_dir.join(format!("config_replica_{count}.toml"));
        if replica_config.exists() {
            count += 1;
        } else {
            break;
        }
    }
    Ok(count)
}

/// Build a NodeInfo from a list of config paths (one per version).
fn build_node_info(
    node_type: NodeType,
    config_paths: Vec<PathBuf>,
) -> Result<NodeInfo, TestCaseError> {
    // Extract port and storage from first version
    let first_config = &config_paths[0];
    let (http_port, storage_path) = extract_config_fields(first_config)?;

    // Verify consistency across all versions
    for (version_idx, config_path) in config_paths.iter().enumerate().skip(1) {
        let (port, storage) = extract_config_fields(config_path)?;

        if port != http_port {
            return Err(TestCaseError::CrossVersionMismatch {
                node_type: node_type.to_string(),
                field: "http_port".to_string(),
                v0_value: http_port.to_string(),
                version: version_idx,
                other_value: port.to_string(),
            });
        }

        if storage != storage_path {
            return Err(TestCaseError::CrossVersionMismatch {
                node_type: node_type.to_string(),
                field: "storage_path".to_string(),
                v0_value: storage_path.display().to_string(),
                version: version_idx,
                other_value: storage.display().to_string(),
            });
        }
    }

    Ok(NodeInfo {
        node_type,
        config_paths,
        http_port,
        storage_path,
    })
}

/// Extract HTTP port and storage path from a rollup config file.
fn extract_config_fields(config_path: &Path) -> Result<(u16, PathBuf), TestCaseError> {
    let content = fs::read_to_string(config_path).map_err(|e| TestCaseError::ReadConfig {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    let config: RollupConfig =
        toml::from_str(&content).map_err(|e| TestCaseError::ParseConfig {
            path: config_path.to_path_buf(),
            source: e,
        })?;

    Ok((config.runner.http_config.bind_port, config.storage.path))
}

/// Validate that nodes have no port or storage path conflicts.
fn validate_nodes(nodes: &[NodeInfo]) -> Result<(), TestCaseError> {
    let mut ports: HashMap<u16, NodeType> = HashMap::new();
    let mut storage_paths: HashMap<PathBuf, NodeType> = HashMap::new();

    for node in nodes {
        // Check port uniqueness
        if let Some(existing) = ports.insert(node.http_port, node.node_type) {
            return Err(TestCaseError::PortConflict {
                port: node.http_port,
                node1: existing.to_string(),
                node2: node.node_type.to_string(),
            });
        }

        // Check storage path uniqueness
        if let Some(existing) = storage_paths.insert(node.storage_path.clone(), node.node_type) {
            return Err(TestCaseError::StoragePathConflict {
                path: node.storage_path.clone(),
                node1: existing.to_string(),
                node2: node.node_type.to_string(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_type_display() {
        assert_eq!(NodeType::Master.to_string(), "master");
        assert_eq!(NodeType::Replica(0).to_string(), "replica_0");
        assert_eq!(NodeType::Replica(5).to_string(), "replica_5");
    }
}
