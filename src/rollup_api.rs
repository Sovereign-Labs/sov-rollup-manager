//! Rollup API client for querying rollup state.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Timeout for individual HTTP requests to the rollup API.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// Minimal representation of rollup config for extracting HTTP port.
#[derive(Debug, Deserialize)]
struct RollupConfig {
    runner: RunnerConfig,
}

#[derive(Debug, Deserialize)]
struct RunnerConfig {
    http_config: HttpConfig,
}

#[derive(Debug, Deserialize)]
struct HttpConfig {
    bind_port: u16,
}

/// Response wrapper for state value queries.
#[derive(Debug, Deserialize)]
struct ValueResponse<T> {
    value: T,
}

#[derive(Debug, Error)]
pub enum RollupApiError {
    #[error("failed to read rollup config {path}: {source}")]
    ReadConfig {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse rollup config {path}: {source}")]
    ParseConfig {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },

    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("failed to parse response: {0}")]
    ParseResponse(reqwest::Error),
}

/// Client for querying the rollup's HTTP API.
pub struct RollupApiClient {
    client: reqwest::blocking::Client,
    base_url: String,
}

impl RollupApiClient {
    /// Create a new client by parsing the rollup config to determine the port.
    pub fn from_config(config_path: &Path) -> Result<Self, RollupApiError> {
        let port = parse_http_port(config_path)?;
        Ok(Self::new(port))
    }

    /// Create a new client with a known port.
    pub fn new(port: u16) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(1))
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            base_url: format!("http://localhost:{}", port),
        }
    }

    /// Query current rollup height from the chain-state module.
    ///
    /// Returns `(rollup_height, visible_slot_number)`.
    pub fn query_current_heights(&self) -> Result<(u64, u64), RollupApiError> {
        let url = format!(
            "{}/modules/chain-state/state/current-heights/",
            self.base_url
        );

        let response: ValueResponse<(u64, u64)> = self
            .client
            .get(&url)
            .send()?
            .json()
            .map_err(RollupApiError::ParseResponse)?;

        Ok(response.value)
    }

    /// Query just the rollup height (first element of current_heights).
    pub fn query_rollup_height(&self) -> Result<u64, RollupApiError> {
        self.query_current_heights().map(|(height, _)| height)
    }
}

/// Parse the HTTP bind port from a rollup config file.
pub fn parse_http_port(config_path: &Path) -> Result<u16, RollupApiError> {
    let content =
        std::fs::read_to_string(config_path).map_err(|e| RollupApiError::ReadConfig {
            path: config_path.to_path_buf(),
            source: e,
        })?;

    let config: RollupConfig = toml::from_str(&content).map_err(|e| RollupApiError::ParseConfig {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    Ok(config.runner.http_config.bind_port)
}
