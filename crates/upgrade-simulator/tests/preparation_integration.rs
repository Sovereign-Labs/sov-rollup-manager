use std::path::PathBuf;

use tempfile::TempDir;
use upgrade_simulator::{RollupBuilder, load_test_case};
use workspace_test_utils::{init_local_rollup_repo, write_file, write_rollup_config};

#[test]
fn upgrade_simulator_preparation_pipeline_builds_and_resolves_versions() {
    let repo = init_local_rollup_repo();
    let test_root = TempDir::new().expect("test root");
    let test_case_dir = test_root.path().join("local-test");

    // Create test case structure
    write_file(&test_case_dir.join("genesis.json"), "{}");
    write_rollup_config(&test_case_dir.join("v0/config.toml"), "node-data", 12345);
    write_rollup_config(&test_case_dir.join("v1/config.toml"), "node-data", 12345);

    write_file(
        &test_case_dir.join("test_case.toml"),
        &format!(
            r#"repo_url = "{}"

[soak_testing]
num_workers = 2
salt = 7

[[versions]]
commit = "{}"
stop_height = 10

[[versions]]
commit = "{}"
start_height = 11
stop_height = 20
"#,
            repo.repo_url(),
            &repo.commit,
            &repo.commit,
        ),
    );

    // Load test case using existing parser/discovery logic
    let test_case = load_test_case(&test_case_dir).expect("load test case");
    assert_eq!(test_case.name, "local-test");
    assert_eq!(test_case.versions.len(), 2);
    assert_eq!(test_case.nodes.len(), 1);

    // Build binaries via the extracted shared builder crate (through re-export)
    let cache_dir = TempDir::new().expect("cache dir");
    let builder = RollupBuilder::with_repo_url(PathBuf::from(cache_dir.path()), repo.repo_url());

    let node_versions = test_case
        .build_node_versions(&builder)
        .expect("build node versions");
    assert_eq!(node_versions.nodes.len(), 1);
    assert_eq!(node_versions.nodes[0].len(), 2);
    assert!(node_versions.nodes[0][0].rollup_binary.exists());
    assert!(node_versions.nodes[0][1].rollup_binary.exists());

    let soak_config = test_case
        .build_soak_config(&builder)
        .expect("build soak config")
        .expect("soak config should be enabled");
    assert_eq!(soak_config.versions.len(), 2);
    assert!(soak_config.versions[0].0.exists());
    assert!(soak_config.versions[1].0.exists());

    // Optional mock-da build path is available too.
    let mock_da_binary = builder
        .get_mock_da_binary(&repo.commit)
        .expect("build mock-da binary");
    assert!(mock_da_binary.exists());
}

#[test]
fn upgrade_simulator_preparation_pipeline_multinode_layout() {
    let repo = init_local_rollup_repo();
    let test_root = TempDir::new().expect("test root");
    let test_case_dir = test_root.path().join("multinode-test");

    write_file(&test_case_dir.join("genesis.json"), "{}");

    write_rollup_config(
        &test_case_dir.join("v0/config_master.toml"),
        "master-data",
        12345,
    );
    write_rollup_config(
        &test_case_dir.join("v0/config_replica_0.toml"),
        "replica-data",
        12346,
    );
    write_rollup_config(
        &test_case_dir.join("v1/config_master.toml"),
        "master-data",
        12345,
    );
    write_rollup_config(
        &test_case_dir.join("v1/config_replica_0.toml"),
        "replica-data",
        12346,
    );

    write_file(
        &test_case_dir.join("test_case.toml"),
        &format!(
            r#"repo_url = "{}"

[[versions]]
commit = "{}"
stop_height = 5

[[versions]]
commit = "{}"
start_height = 6
stop_height = 10
"#,
            repo.repo_url(),
            &repo.commit,
            &repo.commit,
        ),
    );

    let test_case = load_test_case(&test_case_dir).expect("load test case");
    assert_eq!(test_case.name, "multinode-test");
    assert_eq!(test_case.nodes.len(), 2);
    assert_eq!(test_case.num_replicas(), 1);

    let cache_dir = TempDir::new().expect("cache dir");
    let builder = RollupBuilder::with_repo_url(PathBuf::from(cache_dir.path()), repo.repo_url());

    let node_versions = test_case
        .build_node_versions(&builder)
        .expect("build node versions");
    assert_eq!(node_versions.nodes.len(), 2);
    assert_eq!(node_versions.nodes[0].len(), 2);
    assert_eq!(node_versions.nodes[1].len(), 2);
    assert!(node_versions.nodes[0][0].rollup_binary.exists());
    assert!(node_versions.nodes[1][0].rollup_binary.exists());

    // No soak section in this testcase.
    assert!(
        test_case
            .build_soak_config(&builder)
            .expect("build soak config")
            .is_none()
    );
}
