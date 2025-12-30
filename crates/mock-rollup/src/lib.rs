use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default idle time in ms before the mock rollup exits after reaching final height.
pub const DEFAULT_IDLE_TIME_MS: u64 = 500;

/// Rollup config matching the structure expected by the rollup manager.
#[derive(Debug, Deserialize, Serialize)]
pub struct RollupConfig {
    /// Path to the state file that tracks the current block height.
    pub state_file: PathBuf,
    pub runner: RunnerConfig,
    /// Override the stop height from CLI. Simulates a misbehaving rollup
    /// that doesn't respect --stop-at-rollup-height.
    #[serde(default)]
    pub override_stop_height: Option<u64>,
    /// Exit code to return. Defaults to 0 (success).
    #[serde(default)]
    pub exit_code: Option<u8>,
    /// How long to idle (in ms) after reaching final height before exiting.
    /// Defaults to DEFAULT_IDLE_TIME_MS.
    #[serde(default)]
    pub idle_time_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunnerConfig {
    pub http_config: HttpConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HttpConfig {
    pub bind_port: u16,
}

/// State file format for the mock rollup.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct StateFile {
    /// Current block height.
    pub height: u64,
    /// Signals received by the rollup (in order).
    #[serde(default)]
    pub signals: Vec<String>,
}
