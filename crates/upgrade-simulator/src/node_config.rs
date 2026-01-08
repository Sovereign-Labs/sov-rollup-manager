//! Node configuration detection, loading, and validation.
//!
//! This module handles discovering the configuration layout of a test case,
//! determining whether it's a single-node test or a multi-node replication test,
//! and validating that all configurations are consistent.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::info;

use crate::error::TestCaseError;

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
pub struct NodeConfig {
    /// Type of this node.
    pub node_type: NodeType,
    /// Config file paths for each version (indexed by version number).
    pub config_paths: Vec<PathBuf>,
    /// HTTP port for the rollup API (must be same across all versions).
    pub http_port: u16,
    /// Storage path (must be same across all versions).
    pub storage_path: PathBuf,
}

/// Complete node layout for a test case.
///
/// Single-node tests are represented as a master with an empty replicas vec.
#[derive(Debug, Clone)]
pub struct TestNodeLayout {
    /// Master node configuration.
    pub master: NodeConfig,
    /// Replica node configurations (empty for single-node tests).
    pub replicas: Vec<NodeConfig>,
    /// Whether this test case requires postgres (true if there are replicas).
    pub requires_postgres: bool,
}

impl TestNodeLayout {
    /// Get the number of replicas.
    pub fn num_replicas(&self) -> usize {
        self.replicas.len()
    }

    /// Iterate over all nodes (master first, then replicas).
    pub fn all_nodes(&self) -> impl Iterator<Item = &NodeConfig> {
        std::iter::once(&self.master).chain(self.replicas.iter())
    }
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

/// Detect the node layout from a test case directory.
///
/// This examines the config files in each version directory to determine:
/// - Whether this is a single-node test (config.toml) or replication test (config_master.toml + config_replica_N.toml)
/// - The number of replicas
/// - The HTTP port and storage path for each node
///
/// # Arguments
/// * `test_case_root` - Root directory of the test case (contains v0, v1, etc.)
/// * `num_versions` - Number of versions in the test case
///
/// # Returns
/// A `TestNodeLayout` describing all nodes and their configurations.
pub fn detect_node_layout(
    test_case_root: &Path,
    num_versions: usize,
) -> Result<TestNodeLayout, TestCaseError> {
    if num_versions == 0 {
        return Err(TestCaseError::NoVersions);
    }

    // First, detect the mode and replica count from v0
    let v0_dir = test_case_root.join("v0");
    let (is_replication_mode, num_replicas) = detect_mode_and_replicas(&v0_dir)?;
    info!(is_replication_mode, num_replicas, "Detected node layout");

    // Collect config paths for each node across all versions
    let mut master_paths = Vec::with_capacity(num_versions);
    let mut replica_paths: Vec<Vec<PathBuf>> = (0..num_replicas)
        .map(|_| Vec::with_capacity(num_versions))
        .collect();

    for version_idx in 0..num_versions {
        let version_dir = test_case_root.join(format!("v{version_idx}"));

        // Get master config path
        let master_config = if is_replication_mode {
            version_dir.join("config_master.toml")
        } else {
            version_dir.join("config.toml")
        };

        if !master_config.exists() {
            return Err(TestCaseError::ConfigNotFound(master_config));
        }
        master_paths.push(master_config);

        // Get replica config paths
        for (replica_idx, paths) in replica_paths.iter_mut().enumerate() {
            let replica_config = version_dir.join(format!("config_replica_{replica_idx}.toml"));
            if !replica_config.exists() {
                return Err(TestCaseError::ConfigNotFound(replica_config));
            }
            paths.push(replica_config);
        }

        // Verify no extra replicas in this version
        let expected_replicas = count_replica_configs(&version_dir)?;
        if expected_replicas != num_replicas {
            return Err(TestCaseError::ReplicaCountMismatch {
                version: version_idx,
                count: expected_replicas,
                expected: num_replicas,
            });
        }
    }

    // Extract port and storage path from first version's config for each node
    let master = build_node_config(NodeType::Master, master_paths)?;

    let replicas: Vec<NodeConfig> = replica_paths
        .into_iter()
        .enumerate()
        .map(|(idx, paths)| build_node_config(NodeType::Replica(idx), paths))
        .collect::<Result<_, _>>()?;

    let layout = TestNodeLayout {
        master,
        replicas: replicas.clone(),
        requires_postgres: !replicas.is_empty(),
    };

    // Validate the layout
    validate_node_layout(&layout)?;

    Ok(layout)
}

/// Detect whether a version directory uses replication mode and count replicas.
fn detect_mode_and_replicas(version_dir: &Path) -> Result<(bool, usize), TestCaseError> {
    let master_config = version_dir.join("config_master.toml");
    let legacy_config = version_dir.join("config.toml");

    let is_replication_mode = if master_config.exists() {
        true
    } else if legacy_config.exists() {
        false
    } else {
        return Err(TestCaseError::ConfigNotFound(legacy_config));
    };

    let num_replicas = count_replica_configs(version_dir)?;

    // If we found replicas but no config_master.toml, that's an error
    if num_replicas > 0 && !is_replication_mode {
        return Err(TestCaseError::ConfigModeMismatch {
            version: 0,
            mode: "legacy (config.toml)".to_string(),
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

/// Build a NodeConfig from a list of config paths (one per version).
fn build_node_config(
    node_type: NodeType,
    config_paths: Vec<PathBuf>,
) -> Result<NodeConfig, TestCaseError> {
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

    Ok(NodeConfig {
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

/// Validate that a node layout has no conflicts.
///
/// Checks:
/// - No duplicate ports across any nodes
/// - No duplicate storage paths across any nodes
pub fn validate_node_layout(layout: &TestNodeLayout) -> Result<(), TestCaseError> {
    let mut ports: HashMap<u16, NodeType> = HashMap::new();
    let mut storage_paths: HashMap<PathBuf, NodeType> = HashMap::new();

    for node in layout.all_nodes() {
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
