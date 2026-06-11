//! Checkpoint file management for tracking the current running version.
//!
//! The checkpoint file allows the rollup manager to resume from the correct
//! version on restart, rather than starting from version 0.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Checkpoint data stored in the checkpoint file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Index of the version that is/was running (0-indexed).
    pub version_index: usize,

    /// Binary path of the version, used to validate config compatibility.
    pub binary_path: PathBuf,

    /// Whether this version's migration has already completed, so restarts
    /// don't re-run it. Always `false` for versions without a migration.
    ///
    /// Defaults to `false` when absent (checkpoint files written before this
    /// field existed), which at worst re-runs an idempotent migration once.
    #[serde(default)]
    pub migration_completed: bool,
}

/// Configuration for checkpoint file usage.
#[derive(Debug, Clone)]
pub enum CheckpointConfig {
    /// Checkpoint file is enabled at the given path.
    Enabled { path: PathBuf },

    /// Checkpoint file is explicitly disabled.
    Disabled,
}

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("failed to read checkpoint file: {0}")]
    Read(std::io::Error),

    #[error("failed to write checkpoint file: {0}")]
    Write(std::io::Error),

    #[error("failed to parse checkpoint file: {0}")]
    Parse(serde_json::Error),

    #[error("failed to serialize checkpoint: {0}")]
    Serialize(serde_json::Error),
}

/// Load a checkpoint from a file.
///
/// Returns `Ok(None)` if the file doesn't exist (first run).
pub fn load_checkpoint(path: &Path) -> Result<Option<Checkpoint>, CheckpointError> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let checkpoint: Checkpoint =
                serde_json::from_str(&content).map_err(CheckpointError::Parse)?;
            Ok(Some(checkpoint))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CheckpointError::Read(e)),
    }
}

/// Write a checkpoint to a file.
pub fn write_checkpoint(path: &Path, checkpoint: &Checkpoint) -> Result<(), CheckpointError> {
    let content = serde_json::to_string_pretty(checkpoint).map_err(CheckpointError::Serialize)?;
    std::fs::write(path, content).map_err(CheckpointError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_nonexistent_returns_none() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("checkpoint.json");

        let result = load_checkpoint(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn write_and_load_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("checkpoint.json");

        let checkpoint = Checkpoint {
            version_index: 2,
            binary_path: PathBuf::from("/usr/bin/rollup-v3"),
            migration_completed: true,
        };

        write_checkpoint(&path, &checkpoint).unwrap();
        let loaded = load_checkpoint(&path).unwrap().unwrap();

        assert_eq!(loaded.version_index, 2);
        assert_eq!(loaded.binary_path, PathBuf::from("/usr/bin/rollup-v3"));
        assert!(loaded.migration_completed);
    }

    #[test]
    fn missing_migration_completed_defaults_to_false() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("checkpoint.json");

        // Checkpoint file written before the migration_completed field existed
        std::fs::write(
            &path,
            r#"{"version_index": 1, "binary_path": "/usr/bin/rollup-v2"}"#,
        )
        .unwrap();

        let loaded = load_checkpoint(&path).unwrap().unwrap();
        assert_eq!(loaded.version_index, 1);
        assert!(!loaded.migration_completed);
    }

    #[test]
    fn parse_error_on_invalid_json() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("checkpoint.json");

        std::fs::write(&path, "not valid json").unwrap();

        let result = load_checkpoint(&path);
        assert!(matches!(result, Err(CheckpointError::Parse(_))));
    }
}
