use std::path::PathBuf;

use sov_versioned_artifact_builder::{
    BuildRequest, BuildSpec, BuildTargets, VersionBuildSpec, prepare_artifacts,
};
use tempfile::TempDir;
use workspace_test_utils::init_local_rollup_repo;

#[test]
fn prepare_artifacts_builds_rollup_soak_and_mock_da() {
    let repo = init_local_rollup_repo();
    let cache_dir = TempDir::new().expect("cache dir");

    let spec = BuildSpec {
        repo_url: Some(repo.repo_url()),
        targets: BuildTargets::upgrade_simulator_defaults(),
        versions: vec![VersionBuildSpec {
            commit: repo.commit.clone(),
            build_soak: true,
        }],
    };

    let req = BuildRequest {
        cache_dir: PathBuf::from(cache_dir.path()),
        build_soak_binaries: true,
        build_mock_da_binary: true,
    };

    let prepared = prepare_artifacts(&spec, &req).expect("prepare artifacts");
    assert_eq!(prepared.versions.len(), 1);
    assert!(prepared.versions[0].rollup_binary.exists());
    assert!(
        prepared.versions[0]
            .soak_binary
            .as_ref()
            .expect("soak binary")
            .exists()
    );
    assert!(prepared.mock_da_binary.as_ref().expect("mock-da").exists());

    // Second call should reuse cached artifacts and produce same paths.
    let prepared_cached = prepare_artifacts(&spec, &req).expect("prepare artifacts cached");
    assert_eq!(
        prepared.versions[0].rollup_binary,
        prepared_cached.versions[0].rollup_binary
    );
}

#[test]
fn prepare_artifacts_without_mock_da_skips_mock_da_build() {
    let repo = init_local_rollup_repo();
    let cache_dir = TempDir::new().expect("cache dir");

    let spec = BuildSpec {
        repo_url: Some(repo.repo_url()),
        targets: BuildTargets::default(),
        versions: vec![VersionBuildSpec {
            commit: repo.commit.clone(),
            build_soak: false,
        }],
    };

    let req = BuildRequest {
        cache_dir: PathBuf::from(cache_dir.path()),
        build_soak_binaries: false,
        build_mock_da_binary: false,
    };

    let prepared = prepare_artifacts(&spec, &req).expect("prepare artifacts");
    assert_eq!(prepared.versions.len(), 1);
    assert!(prepared.versions[0].rollup_binary.exists());
    assert!(prepared.versions[0].soak_binary.is_none());
    assert!(prepared.mock_da_binary.is_none());
}

#[test]
fn prepare_artifacts_errors_when_soak_is_requested_but_target_is_disabled() {
    let repo = init_local_rollup_repo();
    let cache_dir = TempDir::new().expect("cache dir");

    let spec = BuildSpec {
        repo_url: Some(repo.repo_url()),
        targets: BuildTargets::default(),
        versions: vec![VersionBuildSpec {
            commit: repo.commit.clone(),
            build_soak: true,
        }],
    };

    let req = BuildRequest {
        cache_dir: PathBuf::from(cache_dir.path()),
        build_soak_binaries: false,
        build_mock_da_binary: false,
    };

    let err = prepare_artifacts(&spec, &req).expect_err("soak target should be disabled");
    assert!(format!("{err}").contains("build target 'soak' is disabled"));
}

#[test]
fn prepare_artifacts_errors_when_mock_da_is_requested_but_target_is_disabled() {
    let repo = init_local_rollup_repo();
    let cache_dir = TempDir::new().expect("cache dir");

    let spec = BuildSpec {
        repo_url: Some(repo.repo_url()),
        targets: BuildTargets::default(),
        versions: vec![VersionBuildSpec {
            commit: repo.commit.clone(),
            build_soak: false,
        }],
    };

    let req = BuildRequest {
        cache_dir: PathBuf::from(cache_dir.path()),
        build_soak_binaries: false,
        build_mock_da_binary: true,
    };

    let err = prepare_artifacts(&spec, &req).expect_err("mock-da target should be disabled");
    assert!(format!("{err}").contains("build target 'mock-da-server' is disabled"));
}
