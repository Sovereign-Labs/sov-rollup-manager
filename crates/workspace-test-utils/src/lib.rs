use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

pub struct LocalRollupRepo {
    pub dir: TempDir,
    pub commit: String,
}

impl LocalRollupRepo {
    pub fn repo_url(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }
}

pub fn run_ok(cmd: &mut Command) {
    let output = cmd.output().expect("failed to run command");
    if !output.status.success() {
        panic!(
            "command failed: status={:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

pub fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, content).expect("write file");
}

pub fn write_rollup_config(path: &Path, storage_path: &str, bind_port: u16) {
    write_file(
        path,
        &format!(
            r#"[storage]
path = "{storage_path}"

[runner.http_config]
bind_port = {bind_port}
"#
        ),
    );
}

pub fn init_local_rollup_repo() -> LocalRollupRepo {
    let repo_dir = TempDir::new().expect("create repo dir");

    write_file(
        &repo_dir.path().join("Cargo.toml"),
        r#"[package]
name = "rollup-starter-soak-test"
version = "0.1.0"
edition = "2021"

[features]
default = []
acceptance-testing = []
mock_da_external = []
mock_zkvm = []
"#,
    );

    write_file(
        &repo_dir.path().join("src/bin/rollup.rs"),
        r#"fn main() { println!("rollup"); }"#,
    );
    write_file(
        &repo_dir.path().join("src/bin/rollup-starter-soak-test.rs"),
        r#"fn main() { println!("soak"); }"#,
    );
    write_file(
        &repo_dir.path().join("src/bin/mock-da-server.rs"),
        r#"fn main() { println!("mock-da"); }"#,
    );

    run_ok(Command::new("git").current_dir(repo_dir.path()).arg("init"));
    run_ok(Command::new("git").current_dir(repo_dir.path()).args([
        "config",
        "user.email",
        "test@example.com",
    ]));
    run_ok(Command::new("git").current_dir(repo_dir.path()).args([
        "config",
        "user.name",
        "Test User",
    ]));
    run_ok(
        Command::new("git")
            .current_dir(repo_dir.path())
            .args(["add", "."]),
    );
    run_ok(
        Command::new("git")
            .current_dir(repo_dir.path())
            .args(["commit", "-m", "initial"]),
    );

    let output = Command::new("git")
        .current_dir(repo_dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    assert!(output.status.success(), "rev-parse failed");

    let commit = String::from_utf8(output.stdout)
        .expect("valid utf8")
        .trim()
        .to_string();

    LocalRollupRepo {
        dir: repo_dir,
        commit,
    }
}

/// Initializes a local repo whose `rollup` binary is a thin wrapper over
/// `MOCK_ROLLUP_BINARY` and whose `mock-da-server` is a long-running no-op process.
///
/// This is useful for end-to-end upgrade-simulator tests where we want manager
/// semantics with minimal repository boilerplate.
pub fn init_manager_compatible_rollup_repo() -> LocalRollupRepo {
    let repo_dir = TempDir::new().expect("create repo dir");

    write_file(
        &repo_dir.path().join("Cargo.toml"),
        r#"[package]
name = "manager-compatible-rollup"
version = "0.1.0"
edition = "2021"

[features]
default = []
acceptance-testing = []
mock_da_external = []
mock_zkvm = []
"#,
    );

    write_file(
        &repo_dir.path().join("src/bin/rollup.rs"),
        r#"use std::process::{Command, ExitCode};

fn normalize_args() -> Vec<String> {
    let mut normalized = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Flags understood by mock-rollup.
            "--rollup-config-path" | "--start-at-rollup-height" | "--stop-at-rollup-height" => {
                normalized.push(arg);
                if let Some(value) = args.next() {
                    normalized.push(value);
                }
            }
            // Flags passed by upgrade-simulator/manager that mock-rollup does not use.
            "--genesis-path" | "--metrics" => {
                let _ = args.next();
            }
            _ => {
                // Ignore unknown passthrough args so this wrapper stays compatible
                // with manager-style command lines.
            }
        }
    }

    normalized
}

fn main() -> ExitCode {
    let mock_rollup = std::env::var("MOCK_ROLLUP_BINARY")
        .expect("MOCK_ROLLUP_BINARY env var must be set");
    let status = Command::new(mock_rollup)
        .args(normalize_args())
        .status()
        .expect("failed to run wrapped mock-rollup binary");

    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::FAILURE,
    }
}
"#,
    );

    write_file(
        &repo_dir.path().join("src/bin/mock-da-server.rs"),
        r#"use std::time::Duration;

fn main() {
    loop {
        std::thread::sleep(Duration::from_millis(250));
    }
}
"#,
    );

    run_ok(Command::new("git").current_dir(repo_dir.path()).arg("init"));
    run_ok(Command::new("git").current_dir(repo_dir.path()).args([
        "config",
        "user.email",
        "test@example.com",
    ]));
    run_ok(Command::new("git").current_dir(repo_dir.path()).args([
        "config",
        "user.name",
        "Test User",
    ]));
    run_ok(
        Command::new("git")
            .current_dir(repo_dir.path())
            .args(["add", "."]),
    );
    run_ok(
        Command::new("git")
            .current_dir(repo_dir.path())
            .args(["commit", "-m", "initial"]),
    );

    let output = Command::new("git")
        .current_dir(repo_dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    assert!(output.status.success(), "rev-parse failed");

    let commit = String::from_utf8(output.stdout)
        .expect("valid utf8")
        .trim()
        .to_string();

    LocalRollupRepo {
        dir: repo_dir,
        commit,
    }
}
