//! Error types for the upgrade simulator.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::builder::BuilderError;

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
}
