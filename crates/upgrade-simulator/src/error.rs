//! Error types for the upgrade simulator.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;

use crate::builder::BuilderError;
use crate::docker::DockerError;

/// Errors that can occur when running a test case.
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

    #[error("failed to write interpolated config {path}: {source}")]
    WriteConfig { path: PathBuf, source: io::Error },

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
    HttpPortParse(#[from] sov_rollup_manager::RollupApiError),

    #[error("failed to start soak-test process: {0}")]
    SoakTestStartFailed(io::Error),

    #[error("soak-test process failed with exit code {exit_code}")]
    SoakTestFailed { exit_code: i32 },

    #[error("manager task panicked: {0}")]
    ManagerTaskPanic(tokio::task::JoinError),

    // Docker errors
    #[error("docker operation failed: {0}")]
    Docker(#[from] DockerError),

    // Validation errors
    #[error("test case has no versions defined")]
    NoVersions,

    #[error("port {port} is used by multiple nodes: {node1} and {node2}")]
    PortConflict {
        port: u16,
        node1: String,
        node2: String,
    },

    #[error("storage path '{path}' is used by multiple nodes: {node1} and {node2}")]
    StoragePathConflict {
        path: PathBuf,
        node1: String,
        node2: String,
    },

    #[error("version {version} has {count} replica configs, but version 0 has {expected}")]
    ReplicaCountMismatch {
        version: usize,
        count: usize,
        expected: usize,
    },

    #[error(
        "inconsistent config mode: version {version} uses {mode}, but expected {expected_mode}"
    )]
    ConfigModeMismatch {
        version: usize,
        mode: String,
        expected_mode: String,
    },

    #[error(
        "{node_type} node has different {field} across versions: v0={v0_value}, v{version}={other_value}"
    )]
    CrossVersionMismatch {
        node_type: String,
        field: String,
        v0_value: String,
        version: usize,
        other_value: String,
    },

    // Multi-node / process spawning errors
    #[error("{node_type} node failed: {source}")]
    NodeFailed {
        node_type: String,
        #[source]
        source: Box<TestCaseError>,
    },

    #[error("{node_type} node task panicked: {source}")]
    NodeTaskPanic {
        node_type: String,
        source: tokio::task::JoinError,
    },

    #[error("failed to spawn {node_type} rollup-manager process: {source}")]
    ManagerSpawn {
        node_type: String,
        source: io::Error,
    },

    #[error("{node_type} rollup-manager process exited with status {status:?}")]
    ManagerNonZeroExit {
        node_type: String,
        status: ExitStatus,
    },

    #[error("failed to wait for {node_type} rollup-manager process: {source}")]
    ManagerWait {
        node_type: String,
        source: io::Error,
    },

    #[error("failed to write manager config for {node_type}: {source}")]
    WriteManagerConfig {
        node_type: String,
        source: io::Error,
    },

    #[error("failed to build rollup-manager binary: {0}")]
    BuildManager(io::Error),

    #[error("failed to serialize manager config: {0}")]
    SerializeManagerConfig(serde_json::Error),

    #[error("test interrupted by signal (Ctrl+C)")]
    Interrupted,

    #[error("failed to spawn mock-da-server: {0}")]
    MockDaSpawn(io::Error),
}
