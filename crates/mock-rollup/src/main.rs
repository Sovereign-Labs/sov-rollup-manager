use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{fs, thread};

use clap::Parser;
use signal_hook::consts::{SIGHUP, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::{error, info};
use tungstenite::Message;

use mock_rollup::{DEFAULT_IDLE_TIME_MS, RollupConfig, StateFile};

/// Maximum time the slot-stream server waits for the manager to subscribe.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time the main thread waits for the slot stream to be delivered
/// before proceeding to (possibly immediately) exit.
const FRAME_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Drain any pending forwarded signals into `received`.
fn drain_signals(signals: &mut Signals, received: &mut Vec<String>) {
    for sig in signals.pending() {
        let name = signal_name(sig);
        info!(signal = name, "Received signal");
        received.push(name.to_string());
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
    info!(
        current_height,
        state_version = state.state_version,
        "Current state loaded"
    );

    // Validate expected state version if configured.
    if let Some(expected_state_version) = config.expected_state_version {
        if state.state_version != expected_state_version {
            return Err(format!(
                "state_version ({}) != expected_state_version ({expected_state_version})",
                state.state_version
            ));
        }
        info!(expected_state_version, "State version check passed");
    }

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

    // Start a WebSocket server that streams slots to the rollup manager, mirroring
    // the Sovereign node's `/ledger/slots/latest/ws` head subscription.
    let bind_addr = format!("127.0.0.1:{}", config.runner.http_config.bind_port);
    let listener =
        TcpListener::bind(&bind_addr).map_err(|e| format!("failed to bind to {bind_addr}: {e}"))?;
    info!(addr = %bind_addr, "Ledger WebSocket server listening");

    let frames_sent = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let server = thread::spawn({
        let frames_sent = Arc::clone(&frames_sent);
        let stop = Arc::clone(&stop);
        move || serve_slot_stream(listener, current_height, final_height, &frames_sent, &stop)
    });

    // Ensure the final slot has been delivered (so we don't exit before the
    // manager observes the height, even when idle time is zero), then idle for the
    // configured time. Record any forwarded signals throughout.
    let frames_deadline = Instant::now() + FRAME_DELIVERY_TIMEOUT;
    while !frames_sent.load(Ordering::Acquire) && Instant::now() < frames_deadline {
        drain_signals(&mut signals, &mut received_signals);
        thread::sleep(Duration::from_millis(10));
    }

    let idle_time = Duration::from_millis(config.idle_time_ms.unwrap_or(DEFAULT_IDLE_TIME_MS));
    let idle_deadline = Instant::now() + idle_time;
    while Instant::now() < idle_deadline {
        drain_signals(&mut signals, &mut received_signals);
        thread::sleep(Duration::from_millis(10));
    }

    // Drain any signals that arrived right at the end, then stop the server.
    drain_signals(&mut signals, &mut received_signals);
    stop.store(true, Ordering::Release);
    let _ = server.join();

    // Update state with final height and received signals
    state.height = final_height;
    state.signals = received_signals;

    // Write state file
    let state_content =
        serde_json::to_string(&state).map_err(|e| format!("failed to serialize state: {e}"))?;
    fs::write(&config.state_file, state_content)
        .map_err(|e| format!("failed to write state file: {e}"))?;

    info!(
        final_height,
        state_version = state.state_version,
        signals = ?state.signals,
        "Processed blocks, state now at height"
    );
    info!("Mock rollup shutting down gracefully");

    Ok(config.exit_code.unwrap_or(0))
}

/// Accept a single WebSocket subscriber and stream slot frames for heights
/// `from + 1 ..= to`, matching the rollup's `/ledger/slots/latest/ws` head stream
/// (each frame is `{"type":"slot","number":<height>}`). Sets `frames_sent` once
/// the final slot has been flushed.
fn serve_slot_stream(
    listener: TcpListener,
    from: u64,
    to: u64,
    frames_sent: &AtomicBool,
    stop: &AtomicBool,
) {
    let Some(stream) = accept_with_timeout(&listener, stop) else {
        return;
    };

    let mut socket = match tungstenite::accept(stream) {
        Ok(socket) => socket,
        Err(e) => {
            error!(error = %e, "mock rollup: websocket handshake failed");
            return;
        }
    };

    for height in (from + 1)..=to {
        let frame = format!(r#"{{"type":"slot","number":{height}}}"#);
        if let Err(e) = socket.write(Message::Text(frame.into())) {
            error!(error = %e, "mock rollup: failed to send slot frame");
            return;
        }
    }
    if let Err(e) = socket.flush() {
        error!(error = %e, "mock rollup: failed to flush slot frames");
        return;
    }
    frames_sent.store(true, Ordering::Release);

    // Close gracefully so the subscriber sees all slots followed by a clean close.
    // The buffered slots are delivered before the socket reports EOF.
    let _ = socket.close(None);
    let _ = socket.flush();
}

/// Accept one TCP connection, polling so we can bail out if `stop` is set or the
/// manager never connects.
fn accept_with_timeout(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    listener
        .set_nonblocking(true)
        .expect("failed to set listener non-blocking");

    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return None;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream
                    .set_nonblocking(false)
                    .expect("failed to set stream blocking");
                return Some(stream);
            }
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                error!(error = %e, "mock rollup: accept failed");
                return None;
            }
        }
    }
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
