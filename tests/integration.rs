use std::path::PathBuf;
use std::{fs, path::Path};

use tempfile::TempDir;

use mock_rollup::DEFAULT_BLOCK_ADVANCE;
use sov_rollup_manager::{ManagerConfig, RollupVersion, RunnerError, run};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn mock_rollup_binary() -> PathBuf {
    project_root()
        .join("target")
        .join("debug")
        .join("mock-rollup")
}

/// Creates a rollup config TOML file that points to the given state file.
fn write_rollup_config(dir: &TempDir, name: &str, state_file: &Path) -> PathBuf {
    let config_path = dir.path().join(format!("{name}.toml"));
    let content = format!("state_file = {:?}", state_file.to_string_lossy());
    fs::write(&config_path, content).expect("failed to write rollup config");
    config_path
}

fn read_state_height(state_file: &PathBuf) -> u64 {
    fs::read_to_string(state_file)
        .expect("state file should exist")
        .trim()
        .parse()
        .expect("state file should contain a number")
}

#[test]
fn test_single_version_no_heights() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let rollup_config = write_rollup_config(&temp_dir, "v1", &state_file);

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: None,
            stop_height: None,
        }],
    };

    run(&config, &[]).expect("runner should succeed");

    // Single version with no stop height: 0 + DEFAULT_BLOCK_ADVANCE
    assert_eq!(read_state_height(&state_file), DEFAULT_BLOCK_ADVANCE);
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
                stop_height: None,
            },
        ],
    };

    run(&config, &[]).expect("runner should succeed");

    // v1: 0->100, v2: no stop so 100 + DEFAULT_BLOCK_ADVANCE
    assert_eq!(read_state_height(&state_file), 100 + DEFAULT_BLOCK_ADVANCE);
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
                stop_height: None,
            },
        ],
    };

    run(&config, &[]).expect("runner should succeed");

    // v1: 0->50, v2: 51->150, v3: no stop so 150 + DEFAULT_BLOCK_ADVANCE
    assert_eq!(read_state_height(&state_file), 150 + DEFAULT_BLOCK_ADVANCE);
}

#[test]
fn test_start_height_mismatch_detected_by_rollup() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let state_file = temp_dir.path().join("state");
    let rollup_config = write_rollup_config(&temp_dir, "v2", &state_file);

    // Pre-populate state to simulate v1 having run already at height 100
    fs::write(&state_file, "100").expect("failed to write state");

    // v2 claims to start at 200, but state is at 100 (expects 101)
    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: rollup_config,
            migration_path: None,
            start_height: Some(200),
            stop_height: None,
        }],
    };

    let result = run(&config, &[]);
    assert!(result.is_err(), "runner should fail due to height mismatch");
}

#[test]
fn test_rollup_failure_propagates_to_manager() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");

    // Point to a non-existent config file - rollup should fail to start
    let nonexistent_config = temp_dir.path().join("does_not_exist.toml");

    let config = ManagerConfig {
        versions: vec![RollupVersion {
            rollup_binary: mock_rollup_binary(),
            config_path: nonexistent_config,
            migration_path: None,
            start_height: None,
            stop_height: None,
        }],
    };

    let result = run(&config, &[]);
    assert!(
        matches!(result, Err(RunnerError::NonZeroExit { .. })),
        "runner should return NonZeroExit when rollup fails: {result:?}"
    );
}
