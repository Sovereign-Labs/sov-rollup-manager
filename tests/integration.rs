use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::{fs, path::Path};

use escargot::CargoBuild;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tempfile::TempDir;

use mock_rollup::{HttpConfig, RollupConfig, RunnerConfig, StateFile};
use sov_rollup_manager::{
    Checkpoint, CheckpointConfig, ManagerConfig, RollupVersion, RunnerError, run, write_checkpoint,
};

/// Allocate a free port by binding to port 0 and reading the assigned port.
///
/// The socket is closed when this function returns, freeing the port for use.
/// This means there's a race condition if another process or test grabs the same port, but this is
/// very unlikely in practice.
fn allocate_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind to ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Returns the path to the mock-rollup binary, building it if necessary.
/// The result is cached so subsequent calls don't rebuild.
fn mock_rollup_binary() -> PathBuf {
    static BINARY_PATH: OnceLock<PathBuf> = OnceLock::new();

    BINARY_PATH
        .get_or_init(|| {
            CargoBuild::new()
                .package("mock-rollup")
                .bin("mock-rollup")
                .run()
                .expect("failed to build mock-rollup")
                .path()
                .to_path_buf()
        })
        .clone()
}

/// Creates a rollup config TOML file that points to the given state file.
fn write_rollup_config(dir: &TempDir, name: &str, state_file: &Path) -> PathBuf {
    write_rollup_config_ext(dir, name, state_file, None, None, None)
}

/// Creates a rollup config with optional test behavior overrides.
fn write_rollup_config_ext(
    dir: &TempDir,
    name: &str,
    state_file: &Path,
    exit_code: Option<u8>,
    override_stop_height: Option<u64>,
    idle_time_ms: Option<u64>,
) -> PathBuf {
    let config_path = dir.path().join(format!("{name}.toml"));
    let config = RollupConfig {
        state_file: state_file.to_path_buf(),
        runner: RunnerConfig {
            http_config: HttpConfig {
                bind_port: allocate_port(),
            },
        },
        exit_code,
        override_stop_height,
        idle_time_ms,
    };
    let content = toml::to_string(&config).expect("failed to serialize config");
    fs::write(&config_path, content).expect("failed to write rollup config");
    config_path
}

fn write_state_file(state_file: &Path, height: u64) {
    let state = StateFile {
        height,
        signals: vec![],
    };
    let content = serde_json::to_string(&state).expect("failed to serialize state");
    fs::write(state_file, content).expect("failed to write state file");
}

fn read_state_file(state_file: &PathBuf) -> StateFile {
    let content = fs::read_to_string(state_file).expect("state file should exist");
    serde_json::from_str(&content).expect("state file should be valid JSON")
}

fn read_state_height(state_file: &PathBuf) -> u64 {
    read_state_file(state_file).height
}

fn read_checkpoint(path: &Path) -> Checkpoint {
    let content = fs::read_to_string(path).expect("checkpoint file should exist");
    serde_json::from_str(&content).expect("checkpoint file should be valid JSON")
}

#[test]
fn test_single_version_with_stop_height() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let rollup_config = write_rollup_config(&temp_dir, "v1", &state_file);

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };

    run(&config, &[], CheckpointConfig::Disabled).expect("runner should succeed");

    assert_eq!(read_state_height(&state_file), 100);
}

#[test]
fn test_two_versions_with_upgrade() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    let rollup_config_v1 = write_rollup_config(&temp_dir, "v1", &state_file);
    let rollup_config_v2 = write_rollup_config(&temp_dir, "v2", &state_file);

    let config = ManagerConfig {
        versions: vec![
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v1,
                migration_path: None,
                start_height: None,
                stop_height: Some(100),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v2,
                migration_path: None,
                start_height: Some(101),
                stop_height: Some(200),
            },
        ],
    };

    run(&config, &[], CheckpointConfig::Disabled).expect("runner should succeed");

    // v1: 0->100, v2: 101->200
    assert_eq!(read_state_height(&state_file), 200);
}

#[test]
fn test_version_that_stops_instantly() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let rollup_config = write_rollup_config(&temp_dir, "v1", &state_file);

    // Pre-populate state to simulate v0 having run already at height 100
    write_state_file(&state_file, 100);

    // v1 should stop with an error
    let config = ManagerConfig {
        versions: vec![
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config.clone(),
                migration_path: None,
                start_height: None,
                stop_height: Some(100),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config,
                migration_path: None,
                start_height: Some(101),
                stop_height: Some(200),
            },
        ],
    };

    let result = run(&config, &[], CheckpointConfig::Disabled);
    let err = result.expect_err("runner should fail when rollup exits with non-zero code");
    assert!(
        matches!(err, RunnerError::NonZeroExit { .. }),
        "runner should return NonZeroExit: {err:?}"
    );
}

#[test]
fn test_three_versions_chain() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    let rollup_config_v1 = write_rollup_config(&temp_dir, "v1", &state_file);
    let rollup_config_v2 = write_rollup_config(&temp_dir, "v2", &state_file);
    let rollup_config_v3 = write_rollup_config(&temp_dir, "v3", &state_file);

    let config = ManagerConfig {
        versions: vec![
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v1,
                migration_path: None,
                start_height: None,
                stop_height: Some(50),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v2,
                migration_path: None,
                start_height: Some(51),
                stop_height: Some(150),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v3,
                migration_path: None,
                start_height: Some(151),
                stop_height: Some(250),
            },
        ],
    };

    run(&config, &[], CheckpointConfig::Disabled).expect("runner should succeed");

    // v1: 0->50, v2: 51->150, v3: 151->250
    assert_eq!(read_state_height(&state_file), 250);
}

#[test]
fn test_start_height_mismatch_detected_by_rollup() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let rollup_config = write_rollup_config(&temp_dir, "v2", &state_file);

    // Pre-populate state to simulate v1 having run already at height 100
    write_state_file(&state_file, 100);

    // v2 claims to start at 200, but state is at 100 (expects 101)
    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: Some(200),
            stop_height: Some(300),
        }],
    };

    let result = run(&config, &[], CheckpointConfig::Disabled);
    let err = result.expect_err("runner should fail when rollup exits with non-zero code");
    assert!(
        matches!(err, RunnerError::NonZeroExit { .. }),
        "runner should return NonZeroExit: {err:?}"
    );
}

fn test_rollup_failure(rollup_exit_at: Option<u64>) {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    // Create a config that tells the mock rollup to exit with code 42
    let rollup_config =
        write_rollup_config_ext(&temp_dir, "v1", &state_file, Some(42), rollup_exit_at, None);

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };

    let result = run(&config, &[], CheckpointConfig::Disabled);
    let err = result.expect_err("runner should fail when rollup exits with non-zero code");
    assert!(
        matches!(err, RunnerError::NonZeroExit { .. }),
        "runner should return NonZeroExit: {err:?}"
    );
    assert_eq!(
        err.exit_code(),
        Some(42),
        "exit code should match rollup's configured exit code"
    );
}

#[test]
fn test_rollup_failure_at_stop_height() {
    test_rollup_failure(None);
}

#[test]
fn test_rollup_failure_before_stop_height() {
    test_rollup_failure(Some(20));
}

#[test]
fn test_rollup_exits_early_with_success() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    // Rollup will exit at height 50 (success), but manager expects it to run until 100
    let rollup_config = write_rollup_config_ext(&temp_dir, "v1", &state_file, None, Some(50), None);

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };

    let result = run(&config, &[], CheckpointConfig::Disabled);
    let err = result.expect_err("runner should fail when rollup exits before stop height");
    assert!(
        matches!(err, RunnerError::PrematureExit { .. }),
        "runner should return PrematureExit: {err:?}"
    );
}

#[test]
fn test_rollup_exceeds_stop_height() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    // Rollup will continue to height 150, but manager expects it to stop at 100
    let rollup_config =
        write_rollup_config_ext(&temp_dir, "v1", &state_file, None, Some(150), None);

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };

    let result = run(&config, &[], CheckpointConfig::Disabled);
    let err = result.expect_err("runner should fail when rollup exceeds stop height");
    assert!(
        matches!(err, RunnerError::ExceededStopHeight { .. }),
        "runner should return ExceededStopHeight: {err:?}"
    );
}

#[test]
fn test_rollup_exits_unexpectedly() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    // Rollup will exit at height 50, but manager has no stop height (expects indefinite run)
    let rollup_config = write_rollup_config_ext(&temp_dir, "v1", &state_file, None, Some(50), None);

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: None,
        }],
    };

    let result = run(&config, &[], CheckpointConfig::Disabled);
    let err = result.expect_err("runner should fail when rollup exits unexpectedly");
    assert!(
        matches!(err, RunnerError::UnexpectedExit { .. }),
        "runner should return UnexpectedExit: {err:?}"
    );
}

/// Returns the path to the rollup-manager binary, building it if necessary.
fn manager_binary() -> PathBuf {
    static BINARY_PATH: OnceLock<PathBuf> = OnceLock::new();

    BINARY_PATH
        .get_or_init(|| {
            CargoBuild::new()
                .package("sov-rollup-manager")
                .bin("sov-rollup-manager")
                .run()
                .expect("failed to build sov-rollup-manager")
                .path()
                .to_path_buf()
        })
        .clone()
}

#[test]
fn test_signal_forwarding() {
    use std::process::{Command, Stdio};

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");

    // Configure the rollup to idle for a couple of seconds, giving us time to send signals
    let rollup_config =
        write_rollup_config_ext(&temp_dir, "v1", &state_file, None, None, Some(1_000));

    // Write manager config
    let manager_config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };
    let manager_config_path = temp_dir.path().join("manager.json");
    let manager_config_content =
        serde_json::to_string(&manager_config).expect("failed to serialize manager config");
    fs::write(&manager_config_path, manager_config_content)
        .expect("failed to write manager config");

    // Spawn the manager as a subprocess
    let mut child = Command::new(manager_binary())
        .args([
            "-c",
            manager_config_path.to_str().unwrap(),
            "--no-checkpoint-file",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn manager");

    let manager_pid = Pid::from_raw(child.id() as i32);

    // Wait for the rollup to start and begin idling
    thread::sleep(Duration::from_millis(500));

    // Send signals to the manager (which should forward them to the rollup)
    signal::kill(manager_pid, Signal::SIGQUIT).expect("failed to send SIGQUIT");
    thread::sleep(Duration::from_millis(100));

    signal::kill(manager_pid, Signal::SIGTERM).expect("failed to send SIGTERM");
    thread::sleep(Duration::from_millis(100));

    signal::kill(manager_pid, Signal::SIGQUIT).expect("failed to send SIGHUP");

    // Wait for the manager to exit
    let status = child.wait().expect("failed to wait for manager");
    assert!(status.success(), "manager should exit successfully");

    // Check the state file for received signals
    let state = read_state_file(&state_file);
    assert_eq!(state.height, 100);

    // Sort signals before comparing to avoid flakiness from race conditions
    let mut actual_signals = state.signals;
    actual_signals.sort();
    let mut expected_signals = vec!["SIGQUIT", "SIGQUIT", "SIGTERM"];
    expected_signals.sort();
    assert_eq!(
        actual_signals, expected_signals,
        "rollup should have received all forwarded signals"
    );
}

#[test]
fn test_checkpoint_enables_version_skip() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let checkpoint_file = temp_dir.path().join("checkpoint.json");

    // Pre-populate state at height 100 (simulating v0 completion)
    write_state_file(&state_file, 100);

    let rollup_config_v0 = write_rollup_config(&temp_dir, "v0", &state_file);
    let rollup_config_v1 = write_rollup_config(&temp_dir, "v1", &state_file);

    // Pre-populate checkpoint file pointing to version 1
    let checkpoint = Checkpoint {
        version_index: 1,
        binary_path: mock_rollup_binary(),
    };
    write_checkpoint(&checkpoint_file, &checkpoint).expect("failed to write checkpoint");

    let config = ManagerConfig {
        versions: vec![
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v0,
                migration_path: None,
                start_height: None,
                stop_height: Some(100),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v1,
                migration_path: None,
                start_height: Some(101),
                stop_height: Some(200),
            },
        ],
    };

    // Run with checkpoint - should skip v0 and run v1
    run(
        &config,
        &[],
        CheckpointConfig::Enabled {
            path: checkpoint_file.clone(),
        },
    )
    .expect("runner should succeed");

    // Verify final state
    assert_eq!(read_state_height(&state_file), 200);

    // Verify checkpoint was updated to v1
    let final_checkpoint = read_checkpoint(&checkpoint_file);
    assert_eq!(final_checkpoint.version_index, 1);
}

#[test]
fn test_checkpoint_binary_mismatch_error() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let checkpoint_file = temp_dir.path().join("checkpoint.json");
    let rollup_config = write_rollup_config(&temp_dir, "v0", &state_file);

    // Pre-populate checkpoint with wrong binary path
    let checkpoint = Checkpoint {
        version_index: 0,
        binary_path: PathBuf::from("/wrong/path/to/rollup"),
    };
    write_checkpoint(&checkpoint_file, &checkpoint).expect("failed to write checkpoint");

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };

    let result = run(
        &config,
        &[],
        CheckpointConfig::Enabled {
            path: checkpoint_file,
        },
    );
    let err = result.expect_err("runner should fail due to binary mismatch");
    assert!(
        matches!(err, RunnerError::CheckpointBinaryMismatch { .. }),
        "runner should return CheckpointBinaryMismatch: {err:?}"
    );
}

#[test]
fn test_checkpoint_index_out_of_bounds_error() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let checkpoint_file = temp_dir.path().join("checkpoint.json");
    let rollup_config = write_rollup_config(&temp_dir, "v0", &state_file);

    // Pre-populate checkpoint with index beyond config versions
    let checkpoint = Checkpoint {
        version_index: 5, // Config only has 1 version
        binary_path: mock_rollup_binary(),
    };
    write_checkpoint(&checkpoint_file, &checkpoint).expect("failed to write checkpoint");

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: Some(100),
        }],
    };

    let result = run(
        &config,
        &[],
        CheckpointConfig::Enabled {
            path: checkpoint_file,
        },
    );
    let err = result.expect_err("runner should fail due to index out of bounds");
    assert!(
        matches!(err, RunnerError::CheckpointIndexOutOfBounds { .. }),
        "runner should return CheckpointIndexOutOfBounds: {err:?}"
    );
}

#[test]
fn test_checkpoint_written_before_each_version() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let checkpoint_file = temp_dir.path().join("checkpoint.json");

    let rollup_config_v0 = write_rollup_config(&temp_dir, "v0", &state_file);
    let rollup_config_v1 = write_rollup_config(&temp_dir, "v1", &state_file);

    let config = ManagerConfig {
        versions: vec![
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v0,
                migration_path: None,
                start_height: None,
                stop_height: Some(50),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v1,
                migration_path: None,
                start_height: Some(51),
                stop_height: Some(100),
            },
        ],
    };

    // Run to completion
    run(
        &config,
        &[],
        CheckpointConfig::Enabled {
            path: checkpoint_file.clone(),
        },
    )
    .expect("runner should succeed");

    // Verify checkpoint points to v1 (last version run)
    let checkpoint = read_checkpoint(&checkpoint_file);
    assert_eq!(checkpoint.version_index, 1);
    assert_eq!(checkpoint.binary_path, mock_rollup_binary());
}

#[test]
fn test_checkpoint_restart_from_intermediate_version() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let checkpoint_file = temp_dir.path().join("checkpoint.json");

    let rollup_config_v0 = write_rollup_config(&temp_dir, "v0", &state_file);
    let rollup_config_v1 = write_rollup_config(&temp_dir, "v1", &state_file);
    let rollup_config_v2 = write_rollup_config(&temp_dir, "v2", &state_file);

    let config = ManagerConfig {
        versions: vec![
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v0.clone(),
                migration_path: None,
                start_height: None,
                stop_height: Some(50),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v1.clone(),
                migration_path: None,
                start_height: Some(51),
                stop_height: Some(100),
            },
            RollupVersion {
                rollup_binary: mock_rollup_binary(),
                config_path: rollup_config_v2.clone(),
                migration_path: None,
                start_height: Some(101),
                stop_height: Some(150),
            },
        ],
    };

    // Simulate: v0 and v1 completed, checkpoint at v2, state at 100
    write_state_file(&state_file, 100);
    let checkpoint = Checkpoint {
        version_index: 2,
        binary_path: mock_rollup_binary(),
    };
    write_checkpoint(&checkpoint_file, &checkpoint).expect("failed to write checkpoint");

    // Run - should skip v0 and v1, run only v2
    run(
        &config,
        &[],
        CheckpointConfig::Enabled {
            path: checkpoint_file.clone(),
        },
    )
    .expect("runner should succeed");

    // Verify final state
    assert_eq!(read_state_height(&state_file), 150);

    // Checkpoint should still be at v2
    let final_checkpoint = read_checkpoint(&checkpoint_file);
    assert_eq!(final_checkpoint.version_index, 2);
}
