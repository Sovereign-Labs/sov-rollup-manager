#![allow(dead_code)]
//! Docker container lifecycle management for postgres.
//!
//! Provides RAII-based container management with cleanup guarantees,
//! ensuring containers are removed even on panics or early returns.
//!
//! The container hosts two databases:
//! - `sequencer`: For rollup soft confirmation storage (reset between test phases)
//! - `mock_da`: For mock DA server storage (persists across test phases)

use std::io;
use std::process::Command;
use std::thread;
use std::time::Duration;

use rand::Rng;
use rand::distr::Alphanumeric;
use tracing::{error, info, warn};

const CONTAINER_NAME: &str = "upgrade_simulator_postgres";
const CONTAINER_PORT: u64 = 5432;

/// Database name for sequencer soft confirmation storage.
const SEQUENCER_DB: &str = "sequencer";
/// Database name for mock DA server storage.
const MOCK_DA_DB: &str = "mock_da";

/// Errors that can occur during docker operations.
#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("failed to start docker container '{name}': {source}")]
    Start { name: String, source: io::Error },

    #[error("docker container '{name}' failed to start (exit code {code:?}): {stderr}")]
    StartFailed {
        name: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("docker container '{name}' failed to become ready after {timeout_secs}s")]
    NotReady { name: String, timeout_secs: u64 },

    #[error("failed to check container readiness '{name}': {source}")]
    ReadinessCheck { name: String, source: io::Error },

    #[error("failed to stop docker container '{name}': {source}")]
    Stop { name: String, source: io::Error },

    #[error("failed to remove docker container '{name}': {source}")]
    Remove { name: String, source: io::Error },

    #[error("failed to execute SQL in container '{name}': {source}")]
    SqlExec { name: String, source: io::Error },

    #[error("SQL command failed in container '{name}' (exit code {code:?}): {stderr}")]
    SqlFailed {
        name: String,
        code: Option<i32>,
        stderr: String,
    },
}

/// RAII guard for a docker container.
///
/// Ensures the container is cleaned up when dropped, even on panics.
/// The `Drop` implementation uses synchronous cleanup to work in all contexts.
pub struct PostgresDockerContainer {
    container_name: String,
    port: u64,
    password: String,
    cleaned_up: bool,
}

impl PostgresDockerContainer {
    /// Start a postgres container and wait for it to be ready.
    ///
    /// This runs `docker run -d`, polls `pg_isready` until the database
    /// is accepting connections, then creates the `sequencer` and `mock_da` databases.
    pub fn start() -> Result<Self, DockerError> {
        let container_name = CONTAINER_NAME.to_string();
        let port = CONTAINER_PORT;
        info!(
            container = %container_name,
            port = port,
            "Starting postgres container"
        );

        let password: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();

        let postgres_env = format!("POSTGRES_PASSWORD={password}");
        let port_mapping = format!("{port}:5432");

        let output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container_name,
                "-e",
                &postgres_env,
                "-p",
                &port_mapping,
                "postgres:16",
            ])
            .output()
            .map_err(|e| DockerError::Start {
                name: container_name.clone(),
                source: e,
            })?;

        if !output.status.success() {
            return Err(DockerError::StartFailed {
                name: container_name.clone(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let container = Self {
            container_name,
            password,
            port,
            cleaned_up: false,
        };

        // Wait for postgres to be ready
        container.wait_for_ready(30)?;

        // Create the two databases
        container.create_databases()?;

        info!(
            container = %container.container_name,
            "Postgres container ready with databases"
        );

        Ok(container)
    }

    /// Connection string for the sequencer database (soft confirmation storage).
    ///
    /// This database is reset between test phases (initial run vs resync).
    pub fn sequencer_connection_string(&self) -> String {
        format!(
            "postgresql://postgres:{}@localhost:{}/{}",
            self.password, self.port, SEQUENCER_DB
        )
    }

    /// Connection string for the mock DA database.
    ///
    /// This database persists across test phases to allow resync from DA data.
    pub fn da_connection_string(&self) -> String {
        format!(
            "postgresql://postgres:{}@localhost:{}/{}",
            self.password, self.port, MOCK_DA_DB
        )
    }

    /// Create the sequencer and mock_da databases.
    fn create_databases(&self) -> Result<(), DockerError> {
        info!(container = %self.container_name, "Creating databases");

        for db_name in [SEQUENCER_DB, MOCK_DA_DB] {
            self.exec_sql(&format!("CREATE DATABASE {db_name}"))?;
            info!(container = %self.container_name, database = db_name, "Created database");
        }

        Ok(())
    }

    /// Execute a SQL command in the container via psql.
    fn exec_sql(&self, sql: &str) -> Result<(), DockerError> {
        let output = Command::new("docker")
            .args([
                "exec",
                &self.container_name,
                "psql",
                "-U",
                "postgres",
                "-c",
                sql,
            ])
            .output()
            .map_err(|e| DockerError::SqlExec {
                name: self.container_name.clone(),
                source: e,
            })?;

        if !output.status.success() {
            return Err(DockerError::SqlFailed {
                name: self.container_name.clone(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Wait for the postgres container to be ready to accept connections.
    fn wait_for_ready(&self, timeout_secs: u64) -> Result<(), DockerError> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        loop {
            if start.elapsed() >= timeout {
                return Err(DockerError::NotReady {
                    name: self.container_name.clone(),
                    timeout_secs,
                });
            }

            let output = Command::new("docker")
                .args(["exec", &self.container_name, "pg_isready", "-U", "postgres"])
                .output()
                .map_err(|e| DockerError::ReadinessCheck {
                    name: self.container_name.clone(),
                    source: e,
                })?;

            if output.status.success() {
                return Ok(());
            }

            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Stop and remove the container.
    ///
    /// This is idempotent - calling it multiple times is safe.
    pub fn cleanup(&mut self) -> Result<(), DockerError> {
        if self.cleaned_up {
            return Ok(());
        }

        self.cleanup_sync()?;
        self.cleaned_up = true;
        Ok(())
    }

    /// Reset the sequencer database for a fresh resync test.
    ///
    /// This drops and recreates only the sequencer database, preserving
    /// the mock_da database which contains DA data needed for resync.
    ///
    /// This is much faster than recreating the entire container.
    pub fn reset_sequencer_db(&mut self) -> Result<(), DockerError> {
        info!(container = %self.container_name, "Resetting sequencer database");

        // Terminate any connections to the sequencer database
        // This is required before we can drop it
        self.exec_sql(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{SEQUENCER_DB}'"
        ))?;

        // Drop and recreate the sequencer database
        self.exec_sql(&format!("DROP DATABASE IF EXISTS {SEQUENCER_DB}"))?;
        self.exec_sql(&format!("CREATE DATABASE {SEQUENCER_DB}"))?;

        info!(container = %self.container_name, "Sequencer database reset");

        Ok(())
    }

    /// Internal synchronous cleanup implementation.
    fn cleanup_sync(&self) -> Result<(), DockerError> {
        info!(container = %self.container_name, "Stopping postgres container");

        // Stop the container (ignore errors - it may already be stopped)
        let stop_result = Command::new("docker")
            .args(["stop", &self.container_name])
            .output();

        if let Err(e) = stop_result {
            warn!(
                container = %self.container_name,
                error = %e,
                "Failed to stop container (may already be stopped)"
            );
        }

        // Remove the container
        info!(container = %self.container_name, "Removing postgres container");

        let remove_output = Command::new("docker")
            .args(["rm", "-f", &self.container_name])
            .output()
            .map_err(|e| DockerError::Remove {
                name: self.container_name.clone(),
                source: e,
            })?;

        if !remove_output.status.success() {
            // Log but don't fail - the container may not exist
            let stderr = String::from_utf8_lossy(&remove_output.stderr);
            warn!(
                container = %self.container_name,
                stderr = %stderr,
                "Container removal returned non-zero (may already be removed)"
            );
        }

        Ok(())
    }

    /// Get the container name.
    pub fn name(&self) -> &str {
        &self.container_name
    }
}

impl Drop for PostgresDockerContainer {
    fn drop(&mut self) {
        if !self.cleaned_up {
            warn!(
                container = %self.container_name,
                "DockerContainer dropped without explicit cleanup, cleaning up now"
            );
            if let Err(e) = self.cleanup_sync() {
                error!(
                    container = %self.container_name,
                    error = %e,
                    "Failed to cleanup container in Drop"
                );
            }
            self.cleaned_up = true;
        }
    }
}
