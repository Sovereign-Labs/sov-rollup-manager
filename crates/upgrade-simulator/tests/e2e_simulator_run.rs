use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use escargot::CargoBuild;
use tempfile::TempDir;
use upgrade_simulator::{load_test_case, run_test_case};
use workspace_test_utils::{init_manager_compatible_rollup_repo, write_file};

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

fn assert_interpolated_config(path: PathBuf) {
    let content = fs::read_to_string(&path).expect("read interpolated config");

    assert!(
        !content.contains("{postgres_connection_string}"),
        "placeholder should be replaced in {}",
        path.display()
    );
    assert!(
        !content.contains("{mock_da_url}"),
        "placeholder should be replaced in {}",
        path.display()
    );
    assert!(
        content.contains("@localhost:5432/sequencer"),
        "interpolated postgres URL should be present in {}",
        path.display()
    );
    assert!(
        content.contains("http://localhost:50051"),
        "interpolated mock DA URL should be present in {}",
        path.display()
    );
}

#[test]
/// Requires docker.
fn e2e_upgrade_simulator_run_with_mock_rollup_wrapper() {
    let repo = init_manager_compatible_rollup_repo();
    let test_root = TempDir::new().expect("test root");
    let test_case_dir = test_root.path().join("e2e-case");
    let cache_dir = TempDir::new().expect("cache dir");

    write_file(&test_case_dir.join("genesis.json"), "{}");

    // Include both storage.path (for upgrade-simulator parsing) and state_file
    // (for mock-rollup runtime behavior).
    write_file(
        &test_case_dir.join("v0/config.toml"),
        r#"state_file = "node-data/state.json"
sequencer_db_url = "{postgres_connection_string}"
da_url = "{mock_da_url}"

[storage]
path = "node-data"

[runner.http_config]
bind_port = 12400
"#,
    );
    write_file(
        &test_case_dir.join("v1/config.toml"),
        r#"state_file = "node-data/state.json"
sequencer_db_url = "{postgres_connection_string}"
da_url = "{mock_da_url}"

[storage]
path = "node-data"

[runner.http_config]
bind_port = 12400
"#,
    );

    write_file(
        &test_case_dir.join("test_case.toml"),
        &format!(
            r#"repo_url = "{}"
extra_blocks_after_resync = 0

[[versions]]
commit = "{}"
stop_height = 3

[[versions]]
commit = "{}"
start_height = 4
stop_height = 6
"#,
            repo.repo_url(),
            &repo.commit,
            &repo.commit,
        ),
    );

    let test_case = load_test_case(&test_case_dir).expect("load test case");

    // SAFETY: the project uses `nextest` for testing, which isolates test processes and makes it
    // safe to set environment variables.
    unsafe {
        std::env::set_var("MOCK_ROLLUP_BINARY", mock_rollup_binary());
    }

    let rt = tokio::runtime::Runtime::new().expect("create runtime");
    rt.block_on(run_test_case(
        cache_dir.path(),
        test_root.path(),
        &test_case,
    ))
    .expect("upgrade simulator end-to-end run should succeed");

    let run_dir = test_case_dir.join("run-dir").join("master");
    assert_interpolated_config(run_dir.join("config_0.toml"));
    assert_interpolated_config(run_dir.join("config_1.toml"));
}
