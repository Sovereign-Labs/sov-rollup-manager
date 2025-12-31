use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fs, thread};

use clap::Parser;
use signal_hook::consts::{SIGHUP, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use tiny_http::{Response, Server};
use tracing::{error, info};

use mock_rollup::{DEFAULT_IDLE_TIME_MS, RollupConfig, StateFile};

#[derive(Parser)]
#[command(name = "mock-rollup")]
#[command(about = "Mock rollup binary for integration testing")]
struct Cli {
    /// Path to the rollup config file (TOML)
    #[arg(long)]
    rollup_config_path: PathBuf,

    /// Block height at which to start processing (skips blocks before this)
    #[arg(long)]
    start_at_rollup_height: Option<u64>,

    /// Block height at which to stop processing and exit
    #[arg(long)]
    stop_at_rollup_height: Option<u64>,
}

fn signal_name(sig: i32) -> &'static str {
    match sig {
        SIGTERM => "SIGTERM",
        SIGQUIT => "SIGQUIT",
        SIGHUP => "SIGHUP",
        _ => "UNKNOWN",
    }
}

fn run() -> Result<u8, String> {
    let cli = Cli::parse();

    // Load rollup config
    let config_content = fs::read_to_string(&cli.rollup_config_path)
        .map_err(|e| format!("failed to read rollup config: {e}"))?;
    let config: RollupConfig = toml::from_str(&config_content)
        .map_err(|e| format!("failed to parse rollup config: {e}"))?;

    // Read current state from state file (default if doesn't exist)
    let mut state: StateFile = if config.state_file.exists() {
        let content = fs::read_to_string(&config.state_file)
            .map_err(|e| format!("failed to read state file: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("failed to parse state file: {e}"))?
    } else {
        StateFile::default()
    };

    let current_height = state.height;
    info!(current_height, "Current state height");

    // Validate start_at_height if provided
    if let Some(start_height) = cli.start_at_rollup_height {
        let expected = current_height + 1;
        if start_height != expected {
            return Err(format!(
                "start_at_height ({start_height}) != current_height + 1 ({expected})"
            ));
        }
        info!(start_height, "Starting at height");
    } else {
        info!("Starting from genesis");
    }

    // Determine final height (config override takes precedence over CLI)
    let final_height = if let Some(override_height) = config.override_stop_height {
        info!(
            override_height,
            cli_stop_height = ?cli.stop_at_rollup_height,
            "Using override stop height from config (ignoring CLI)"
        );
        override_height
    } else if let Some(stop_height) = cli.stop_at_rollup_height {
        info!(stop_height, "Will stop at height");
        stop_height
    } else {
        return Err("Mock rollup was invoked with no stop height specified through either the CLI or the config. A stop height must be provided for the mock rollup to run. This is a bug in the test setup.".to_string());
    };

    // Match sovereign SDK behaviour
    if final_height <= current_height {
        info!(current_height, final_height, exit_code = ?config.exit_code, "Stop height was configued to be less than current state height - exiting with an error (unless exit code was overriden by config)");
        return Ok(config.exit_code.unwrap_or(1));
    }

    // Register signal handlers
    let mut signals = Signals::new([SIGTERM, SIGQUIT, SIGHUP])
        .map_err(|e| format!("failed to register signal handlers: {e}"))?;
    let mut received_signals: Vec<String> = Vec::new();

    // Start HTTP server for height queries
    let bind_addr = format!("127.0.0.1:{}", config.runner.http_config.bind_port);
    let server = Server::http(&bind_addr)
        .map_err(|e| format!("failed to start HTTP server on {bind_addr}: {e}"))?;
    info!(addr = %bind_addr, "HTTP server listening");

    // Shared state for the current height (used by HTTP handler)
    let height = Arc::new(AtomicU64::new(current_height));

    // Spawn HTTP handler thread
    let http_height = Arc::clone(&height);
    let _http_thread = thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url();
            if url.starts_with("/modules/chain-state/state/current-heights") {
                let h = http_height.load(Ordering::Relaxed);
                // Response format: {"value": [rollup_height, visible_slot_number]}
                let body = format!(r#"{{"value": [{h}, {h}]}}"#);
                let response = Response::from_string(body)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    )
                    .with_status_code(200);
                let _ = request.respond(response);
            } else {
                let response = Response::from_string("Not Found").with_status_code(404);
                let _ = request.respond(response);
            }
        }
    });

    // Simulate block processing - jump to final height immediately
    height.store(final_height, Ordering::Relaxed);

    // Idle for configured time while checking for signals
    let idle_time = Duration::from_millis(config.idle_time_ms.unwrap_or(DEFAULT_IDLE_TIME_MS));
    let start = Instant::now();

    while start.elapsed() < idle_time {
        // Check for signals
        for sig in signals.pending() {
            let name = signal_name(sig);
            info!(signal = name, "Received signal");
            received_signals.push(name.to_string());
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Update state with final height and received signals
    state.height = final_height;
    state.signals = received_signals;

    // Write state file
    let state_content =
        serde_json::to_string(&state).map_err(|e| format!("failed to serialize state: {e}"))?;
    fs::write(&config.state_file, state_content)
        .map_err(|e| format!("failed to write state file: {e}"))?;

    info!(final_height, signals = ?state.signals, "Processed blocks, state now at height");
    info!("Mock rollup shutting down gracefully");

    Ok(config.exit_code.unwrap_or(0))
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            error!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
