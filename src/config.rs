use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single rollup version configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupVersion {
    /// Path to the rollup binary for this version.
    pub rollup_binary: PathBuf,

    /// Path to the rollup config file for this version.
    pub config_path: PathBuf,

    /// Optional path to a migration binary/script to run before starting this version.
    /// This is run after the previous version stops and before this version starts.
    #[serde(default)]
    pub migration_path: Option<PathBuf>,

    /// The block height at which this version starts processing.
    /// Must be None for the first version, required for all others.
    #[serde(default)]
    pub start_height: Option<u64>,

    /// The block height at which this version stops processing.
    /// Required for all versions except the last, optional for the last.
    #[serde(default)]
    pub stop_height: Option<u64>,
}

/// The top-level manager configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerConfig {
    /// Ordered list of rollup versions, from oldest to newest.
    pub versions: Vec<RollupVersion>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    ReadFile(#[from] std::io::Error),

    #[error("failed to parse config file: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("config must contain at least one version")]
    NoVersions,

    #[error("first version must not have a start_height")]
    FirstVersionHasStartHeight,

    #[error("version {index} (0-indexed) is missing start_height")]
    MissingStartHeight { index: usize },

    #[error("version {index} (0-indexed) is missing stop_height")]
    MissingStopHeight { index: usize },

    #[error(
        "height gap between version {prev_index} and {next_index}: \
         version {prev_index} stops at {stop_height}, but version {next_index} starts at {start_height} \
         (expected {expected_start})"
    )]
    HeightGap {
        prev_index: usize,
        next_index: usize,
        stop_height: u64,
        start_height: u64,
        expected_start: u64,
    },
}

impl ManagerConfig {
    /// Load and validate a config from a JSON file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate that the config is well-formed.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.versions.is_empty() {
            return Err(ConfigError::NoVersions);
        }

        let first = &self.versions[0];

        // First version must not have start_height
        if first.start_height.is_some() {
            return Err(ConfigError::FirstVersionHasStartHeight);
        }

        // For single-version configs, we're done
        if self.versions.len() == 1 {
            return Ok(());
        }

        // First version must have stop_height (since it's not the last)
        if first.stop_height.is_none() {
            return Err(ConfigError::MissingStopHeight { index: 0 });
        }

        // Check intermediate versions and height continuity
        for i in 1..self.versions.len() {
            let prev = &self.versions[i - 1];
            let curr = &self.versions[i];

            // Current version must have start_height (since it's not the first)
            let start_height = curr
                .start_height
                .ok_or(ConfigError::MissingStartHeight { index: i })?;

            // If not the last version, must have stop_height
            if i < self.versions.len() - 1 && curr.stop_height.is_none() {
                return Err(ConfigError::MissingStopHeight { index: i });
            }

            // Check height continuity: prev.stop_height + 1 == curr.start_height
            let prev_stop = prev.stop_height.unwrap(); // Safe: checked above
            let expected_start = prev_stop + 1;

            if start_height != expected_start {
                return Err(ConfigError::HeightGap {
                    prev_index: i - 1,
                    next_index: i,
                    stop_height: prev_stop,
                    start_height,
                    expected_start,
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(start: Option<u64>, stop: Option<u64>) -> RollupVersion {
        RollupVersion {
            rollup_binary: PathBuf::from("/bin/rollup"),
            config_path: PathBuf::from("/etc/rollup.toml"),
            migration_path: None,
            start_height: start,
            stop_height: stop,
        }
    }

    #[test]
    fn single_version_valid() {
        let config = ManagerConfig {
            versions: vec![version(None, None)],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn two_versions_valid() {
        let config = ManagerConfig {
            versions: vec![version(None, Some(100)), version(Some(101), None)],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn three_versions_valid() {
        let config = ManagerConfig {
            versions: vec![
                version(None, Some(100)),
                version(Some(101), Some(200)),
                version(Some(201), None),
            ],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_versions_error() {
        let config = ManagerConfig { versions: vec![] };
        assert!(matches!(config.validate(), Err(ConfigError::NoVersions)));
    }

    #[test]
    fn first_with_start_height_error() {
        let config = ManagerConfig {
            versions: vec![version(Some(0), None)],
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::FirstVersionHasStartHeight)
        ));
    }

    #[test]
    fn last_with_stop_height_valid() {
        // Last version is allowed to have a stop_height
        let config = ManagerConfig {
            versions: vec![version(None, Some(100)), version(Some(101), Some(200))],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn missing_start_height_error() {
        let config = ManagerConfig {
            versions: vec![version(None, Some(100)), version(None, None)],
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::MissingStartHeight { index: 1 })
        ));
    }

    #[test]
    fn missing_stop_height_error() {
        let config = ManagerConfig {
            versions: vec![
                version(None, None), // Missing stop_height but not last
                version(Some(101), None),
            ],
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::MissingStopHeight { index: 0 })
        ));
    }

    #[test]
    fn height_gap_error() {
        let config = ManagerConfig {
            versions: vec![
                version(None, Some(100)),
                version(Some(102), None), // Should be 101
            ],
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::HeightGap {
                prev_index: 0,
                next_index: 1,
                stop_height: 100,
                start_height: 102,
                expected_start: 101,
            })
        ));
    }

    #[test]
    fn height_overlap_error() {
        let config = ManagerConfig {
            versions: vec![
                version(None, Some(100)),
                version(Some(100), None), // Should be 101, causes overlap
            ],
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::HeightGap { .. })
        ));
    }
}
