//! Rollup API client for querying rollup state.

use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;
use thiserror::Error;
use tracing::debug;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

/// Timeout for connect and read operations.
const TIMEOUT: Duration = Duration::from_secs(1);

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
    Request(#[from] ureq::Error),
}

/// Client for querying the rollup's HTTP API.
pub struct RollupApiClient {
    agent: ureq::Agent,
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
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(TIMEOUT))
            .timeout_recv_body(Some(TIMEOUT))
            .build()
            .new_agent();

        Self {
            agent,
            base_url: format!("http://localhost:{port}"),
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

        let response: ValueResponse<(u64, u64)> =
            self.agent.get(&url).call()?.body_mut().read_json()?;

        Ok(response.value)
    }

    /// Query just the rollup height (first element of current_heights).
    pub fn query_rollup_height(&self) -> Result<u64, RollupApiError> {
        self.query_current_heights().map(|(height, _)| height)
    }
}

/// Parse the HTTP bind port from a rollup config file.
pub fn parse_http_port(config_path: &Path) -> Result<u16, RollupApiError> {
    let content = std::fs::read_to_string(config_path).map_err(|e| RollupApiError::ReadConfig {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    let config: RollupConfig =
        toml::from_str(&content).map_err(|e| RollupApiError::ParseConfig {
            path: config_path.to_path_buf(),
            source: e,
        })?;

    Ok(config.runner.http_config.bind_port)
}

/// Path of the chain-state rollup-height WebSocket subscription on the rollup's HTTP API.
const ROLLUP_HEIGHT_WS_PATH: &str = "/modules/chain-state/rollup-height/ws";

/// How long a subscription read blocks before returning so the worker can
/// re-check its stop flag while no height updates are arriving.
const SUBSCRIPTION_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Delay between reconnection attempts while the rollup API is still starting up.
const SUBSCRIPTION_RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// A background subscription to the rollup's chain-state rollup-height stream
/// that tracks the latest committed rollup height.
///
/// Unlike polling the height endpoint, a subscription receives *every* committed
/// rollup height over a single TCP connection, so the final height before shutdown is
/// observed reliably even when the rollup resyncs and exits within a single poll
/// interval (the kernel delivers all buffered frames before the socket reports EOF).
pub struct HeightSubscription {
    latest_height: Arc<AtomicU64>,
    seen_any: Arc<AtomicBool>,
    stream_ended: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HeightSubscription {
    /// The latest rollup height observed over the subscription, or `None` if no
    /// height has been received yet.
    pub fn latest_height(&self) -> Option<u64> {
        if self.seen_any.load(Ordering::Acquire) {
            Some(self.latest_height.load(Ordering::Acquire))
        } else {
            None
        }
    }

    /// Whether the subscription stream has ended (the socket closed, typically
    /// because the rollup process exited).
    pub fn stream_ended(&self) -> bool {
        self.stream_ended.load(Ordering::Acquire)
    }

    /// Block until the stream ends (all buffered height updates have been drained) or
    /// `timeout` elapses. Call this once the rollup process has exited so that
    /// [`Self::latest_height`] reflects the true final height.
    pub fn wait_for_stream_end(&self, timeout: Duration) {
        let start = Instant::now();
        while !self.stream_ended() && start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for HeightSubscription {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a background thread that subscribes to the rollup-height stream
/// on `port` and tracks the latest committed height.
pub fn spawn_height_subscription(port: u16) -> HeightSubscription {
    let latest_height = Arc::new(AtomicU64::new(0));
    let seen_any = Arc::new(AtomicBool::new(false));
    let stream_ended = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let url = format!("ws://localhost:{port}{ROLLUP_HEIGHT_WS_PATH}");
    let handle = thread::spawn({
        let latest_height = Arc::clone(&latest_height);
        let seen_any = Arc::clone(&seen_any);
        let stream_ended = Arc::clone(&stream_ended);
        let stop = Arc::clone(&stop);
        move || run_subscription(&url, &latest_height, &seen_any, &stream_ended, &stop)
    });

    HeightSubscription {
        latest_height,
        seen_any,
        stream_ended,
        stop,
        handle: Some(handle),
    }
}

/// Connect to the rollup-height stream and forward observed heights into the
/// shared atomics until the socket closes or `stop` is set.
fn run_subscription(
    url: &str,
    latest_height: &AtomicU64,
    seen_any: &AtomicBool,
    stream_ended: &AtomicBool,
    stop: &AtomicBool,
) {
    // Connect, retrying while the rollup API is still starting up.
    let mut socket = loop {
        if stop.load(Ordering::Acquire) {
            stream_ended.store(true, Ordering::Release);
            return;
        }
        match tungstenite::connect(url) {
            Ok((socket, _response)) => break socket,
            Err(error) => {
                debug!(%error, "Height subscription not ready yet, retrying");
                thread::sleep(SUBSCRIPTION_RECONNECT_DELAY);
            }
        }
    };

    // Use a read timeout so the worker can observe `stop` even when no height
    // updates are arriving.
    if let MaybeTlsStream::Plain(tcp) = socket.get_ref() {
        let _ = tcp.set_read_timeout(Some(SUBSCRIPTION_READ_TIMEOUT));
    }

    while !stop.load(Ordering::Acquire) {
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(height) = serde_json::from_str::<u64>(text.as_str()) {
                    latest_height.store(height, Ordering::Release);
                    seen_any.store(true, Ordering::Release);
                    debug!(height, "Observed rollup height (subscription)");
                }
                // Non-height frames (e.g. server-side error messages) are ignored.
            }
            Ok(Message::Close(_)) => break,
            // Ping/Pong are answered internally by tungstenite; ignore other frames.
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                // Read timed out / was interrupted with no new height: loop to
                // re-check `stop`.
            }
            // Any other error means the connection is gone (the rollup exited).
            Err(_) => break,
        }
    }

    stream_ended.store(true, Ordering::Release);
}
