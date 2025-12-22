use std::process::{Command, ExitStatus};

use thiserror::Error;
use tracing::info;

use crate::config::{ManagerConfig, RollupVersion};

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("failed to spawn binary '{path}': {source}")]
    Spawn {
        path: String,
        source: std::io::Error,
    },

    #[error("binary '{path}' exited with non-zero status: {status}")]
    NonZeroExit { path: String, status: ExitStatus },

    #[error("failed to wait for binary '{path}': {source}")]
    Wait {
        path: String,
        source: std::io::Error,
    },
}

/// Runs all rollup versions in sequence.
pub fn run(config: &ManagerConfig) -> Result<(), RunnerError> {
    for (i, version) in config.versions.iter().enumerate() {
        info!(
            version = i,
            binary = %version.binary_path.display(),
            "Starting version"
        );

        // Run migration if this isn't the first version and migration is specified
        if let Some(ref migration_path) = version.migration_path {
            info!(migration = %migration_path.display(), "Running migration");
            run_binary(migration_path.to_str().unwrap(), &[])?;
        }

        // Build arguments for the rollup binary
        let args = build_rollup_args(version);
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        run_binary(version.binary_path.to_str().unwrap(), &args_refs)?;

        info!(version = i, "Version completed successfully");
    }

    info!("All versions completed");
    Ok(())
}

fn build_rollup_args(version: &RollupVersion) -> Vec<String> {
    let mut args = vec![
        "--rollup-config-path".to_string(),
        version.config_path.to_string_lossy().into_owned(),
    ];

    if let Some(start) = version.start_height {
        args.push("--start-at-height".to_string());
        args.push(start.to_string());
    }

    if let Some(stop) = version.stop_height {
        args.push("--stop-at-height".to_string());
        args.push(stop.to_string());
    }

    args
}

fn run_binary(path: &str, args: &[&str]) -> Result<(), RunnerError> {
    let mut child = Command::new(path)
        .args(args)
        .spawn()
        .map_err(|e| RunnerError::Spawn {
            path: path.to_string(),
            source: e,
        })?;

    let status = child.wait().map_err(|e| RunnerError::Wait {
        path: path.to_string(),
        source: e,
    })?;

    if !status.success() {
        return Err(RunnerError::NonZeroExit {
            path: path.to_string(),
            status,
        });
    }

    Ok(())
}
