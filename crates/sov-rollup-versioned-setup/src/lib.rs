use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use sov_versioned_artifact_builder::{
    BuildRequest, BuildSpec, BuildTarget, BuildTargets, RollupBuilder, VersionBuildSpec,
    prepare_artifacts,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const VERSION_SPEC_FILE: &str = "versions.yaml";
pub const VERSION_VARS_COMMIT_KEY: &str = "rollup_commit_hash";
pub const ROLLUP_MANAGER_REPO_URL: &str = "https://github.com/Sovereign-Labs/sov-rollup-manager";
pub const ROLLUP_MANAGER_BRANCH: &str = "master";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinarySource {
    RemoteCommit(String),
    LocalHead,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVersion {
    pub version_id: String,
    pub config_commit: Option<String>,
    pub binary_source: BinarySource,
    pub start_height: Option<u64>,
    pub stop_height: Option<u64>,
    pub migration_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RollupBinaryBuildConfig {
    pub repo_url: String,
    pub cache_dir: PathBuf,
    pub local_rollup_root: PathBuf,
    pub target: BuildTarget,
}

#[derive(Debug, Clone)]
pub struct PreparedRollupVersion {
    pub version_id: String,
    pub config_commit: Option<String>,
    pub binary_source: BinarySource,
    pub rollup_binary: PathBuf,
    pub start_height: Option<u64>,
    pub stop_height: Option<u64>,
    pub migration_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionSpecRoot {
    #[serde(default)]
    rollup_versions: Vec<VersionSpecEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionSpecEntry {
    version_id: String,
    vars_file: PathBuf,
    #[serde(default)]
    start_height: Option<u64>,
    #[serde(default)]
    stop_height: Option<u64>,
    #[serde(default)]
    migration_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionVarsFile {
    rollup_commit_hash: String,
}

pub fn resync_rollup_target() -> BuildTarget {
    BuildTarget {
        package: Some("rollup-starter".to_string()),
        bin: "rollup".to_string(),
        cache_name: Some("rollup-celestia-mock-zkvm".to_string()),
        no_default_features: true,
        features: vec!["celestia_da".to_string(), "mock_zkvm".to_string()],
        extra_args: vec![],
    }
}

pub fn load_versions_replacing_last_with_local_head(
    version_spec_dir: &Path,
) -> Result<Vec<ResolvedVersion>> {
    let spec_path = version_spec_dir.join(VERSION_SPEC_FILE);
    if !spec_path.exists() {
        tracing::info!(
            path = %spec_path.display(),
            "No versions spec found, defaulting to local HEAD only"
        );
        return Ok(vec![ResolvedVersion {
            version_id: "head".to_string(),
            config_commit: None,
            binary_source: BinarySource::LocalHead,
            start_height: None,
            stop_height: None,
            migration_path: None,
        }]);
    }

    let spec_contents = fs::read_to_string(&spec_path)
        .with_context(|| format!("failed to read {}", spec_path.display()))?;
    let spec: VersionSpecRoot = serde_yaml::from_str(&spec_contents)
        .with_context(|| format!("failed to parse {}", spec_path.display()))?;

    let mut versions = Vec::with_capacity(spec.rollup_versions.len().max(1));
    for entry in &spec.rollup_versions {
        let vars_path = absolutize(version_spec_dir, &entry.vars_file);
        let vars_contents = fs::read_to_string(&vars_path).with_context(|| {
            format!(
                "failed to read vars file for version {} at {}",
                entry.version_id,
                vars_path.display()
            )
        })?;
        let vars: VersionVarsFile = serde_yaml::from_str(&vars_contents).with_context(|| {
            format!(
                "failed to parse vars file for version {} at {}",
                entry.version_id,
                vars_path.display()
            )
        })?;
        let commit = vars.rollup_commit_hash.trim();
        if commit.is_empty() {
            return Err(anyhow!(
                "vars file {} for version {} is missing non-empty {}",
                vars_path.display(),
                entry.version_id,
                VERSION_VARS_COMMIT_KEY
            ));
        }

        versions.push(ResolvedVersion {
            version_id: entry.version_id.clone(),
            config_commit: Some(commit.to_string()),
            binary_source: BinarySource::RemoteCommit(commit.to_string()),
            start_height: entry.start_height,
            stop_height: entry.stop_height,
            migration_path: entry
                .migration_path
                .as_ref()
                .map(|path| absolutize(version_spec_dir, path)),
        });
    }

    if versions.is_empty() {
        versions.push(ResolvedVersion {
            version_id: "head".to_string(),
            config_commit: None,
            binary_source: BinarySource::LocalHead,
            start_height: None,
            stop_height: None,
            migration_path: None,
        });
    } else if let Some(last) = versions.last_mut() {
        last.config_commit = None;
        last.binary_source = BinarySource::LocalHead;
    }

    Ok(versions)
}

pub fn prepare_rollup_binaries(
    versions: &[ResolvedVersion],
    config: &RollupBinaryBuildConfig,
) -> Result<Vec<PreparedRollupVersion>> {
    fs::create_dir_all(&config.cache_dir)
        .with_context(|| format!("failed to create {}", config.cache_dir.display()))?;

    let remote_commits: Vec<String> = versions
        .iter()
        .filter_map(|version| match &version.binary_source {
            BinarySource::RemoteCommit(commit) => Some(commit.clone()),
            BinarySource::LocalHead => None,
        })
        .collect();

    let remote_artifacts = if remote_commits.is_empty() {
        Vec::new()
    } else {
        let build_spec = BuildSpec {
            repo_url: Some(config.repo_url.clone()),
            targets: BuildTargets {
                rollup: config.target.clone(),
                soak: None,
                mock_da: None,
            },
            versions: remote_commits
                .iter()
                .map(|commit| VersionBuildSpec {
                    commit: commit.clone(),
                    build_soak: false,
                })
                .collect(),
        };
        let build_request = BuildRequest {
            cache_dir: config.cache_dir.clone(),
            build_soak_binaries: false,
            build_mock_da_binary: false,
        };
        prepare_artifacts(&build_spec, &build_request)
            .context("failed to prepare historical rollup binaries")?
            .versions
    };
    let mut remote_artifacts = remote_artifacts.into_iter();
    let local_head_binary = build_local_rollup_binary(&config.local_rollup_root, &config.target)?;

    let mut prepared = Vec::with_capacity(versions.len());
    for version in versions {
        let rollup_binary = match &version.binary_source {
            BinarySource::RemoteCommit(commit) => remote_artifacts
                .next()
                .ok_or_else(|| anyhow!("missing prepared artifact for remote commit {commit}"))?
                .rollup_binary
                .canonicalize()
                .with_context(|| format!("failed to canonicalize binary for {commit}"))?,
            BinarySource::LocalHead => local_head_binary.clone(),
        };
        prepared.push(PreparedRollupVersion {
            version_id: version.version_id.clone(),
            config_commit: version.config_commit.clone(),
            binary_source: version.binary_source.clone(),
            rollup_binary,
            start_height: version.start_height,
            stop_height: version.stop_height,
            migration_path: version
                .migration_path
                .as_ref()
                .map(|path| path.canonicalize())
                .transpose()
                .with_context(|| {
                    format!(
                        "failed to canonicalize migration for version {}",
                        version.version_id
                    )
                })?,
        });
    }
    Ok(prepared)
}

pub fn read_text_file_at_commit(
    cache_dir: PathBuf,
    repo_url: String,
    commit: &str,
    path: &Path,
) -> Result<String> {
    RollupBuilder::with_repo_url(cache_dir, repo_url)
        .read_text_file_at_commit(commit, path)
        .with_context(|| format!("failed to read {} at {commit}", path.display()))
}

pub fn build_rollup_manager_binary(manager_build_root: &Path) -> Result<PathBuf> {
    build_rollup_manager_binary_from(
        manager_build_root,
        ROLLUP_MANAGER_REPO_URL,
        ROLLUP_MANAGER_BRANCH,
    )
}

pub fn build_rollup_manager_binary_from(
    manager_build_root: &Path,
    repo_url: &str,
    branch: &str,
) -> Result<PathBuf> {
    if manager_build_root.exists() {
        fs::remove_dir_all(manager_build_root)
            .with_context(|| format!("failed to remove {}", manager_build_root.display()))?;
    }
    fs::create_dir_all(manager_build_root)
        .with_context(|| format!("failed to create {}", manager_build_root.display()))?;

    let manager_repo = manager_build_root.join("repo");
    run_checked(
        Command::new("git").args([
            "clone",
            "--depth",
            "1",
            "--branch",
            branch,
            repo_url,
            &manager_repo.display().to_string(),
        ]),
        "clone sov-rollup-manager",
    )?;
    run_checked(
        Command::new("cargo").current_dir(&manager_repo).args([
            "build",
            "--release",
            "--bin",
            "sov-rollup-manager",
        ]),
        "build sov-rollup-manager",
    )?;

    let manager_binary = manager_repo.join("target/release/sov-rollup-manager");
    if !manager_binary.exists() {
        return Err(anyhow!(
            "built manager binary not found at {}",
            manager_binary.display()
        ));
    }
    manager_binary
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", manager_binary.display()))
}

fn build_local_rollup_binary(rollup_root: &Path, target: &BuildTarget) -> Result<PathBuf> {
    let mut args = vec!["build".to_string(), "--release".to_string()];
    if let Some(package) = &target.package {
        args.push("--package".to_string());
        args.push(package.clone());
    }
    args.push("--bin".to_string());
    args.push(target.bin.clone());
    if target.no_default_features {
        args.push("--no-default-features".to_string());
    }
    if !target.features.is_empty() {
        args.push("--features".to_string());
        args.push(target.features.join(","));
    }
    args.extend(target.extra_args.iter().cloned());

    let output = Command::new("cargo")
        .current_dir(rollup_root)
        .args(&args)
        .output()
        .with_context(|| format!("failed to spawn cargo in {}", rollup_root.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "build local HEAD rollup binary failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let binary = rollup_root.join("target").join("release").join(&target.bin);
    if !binary.exists() {
        return Err(anyhow!(
            "local rollup binary not found at {}",
            binary.display()
        ));
    }
    binary
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", binary.display()))
}

fn run_checked(cmd: &mut Command, context: &str) -> Result<()> {
    let output = cmd.output().with_context(|| format!("{context}: spawn"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "{context} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn absolutize(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_versions_file_defaults_to_head() {
        let temp = tempfile::tempdir().unwrap();
        let versions = load_versions_replacing_last_with_local_head(temp.path()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].binary_source, BinarySource::LocalHead);
        assert_eq!(versions[0].start_height, None);
        assert_eq!(versions[0].stop_height, None);
    }

    #[test]
    fn parses_existing_schema_and_replaces_last_binary_with_head() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("version_vars")).unwrap();
        fs::write(
            temp.path().join(VERSION_SPEC_FILE),
            r#"
rollup_versions:
  - version_id: "v0"
    ansible_commit: "ignored"
    vars_file: "version_vars/v0_vars.yaml"
    start_height: null
    stop_height: 510
    migration_path: null
  - version_id: "v1"
    ansible_commit: "ignored"
    vars_file: "version_vars/v1_vars.yaml"
    start_height: 511
    stop_height: null
    migration_path: null
"#,
        )
        .unwrap();
        fs::write(
            temp.path().join("version_vars/v0_vars.yaml"),
            "rollup_commit_hash: abc123\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("version_vars/v1_vars.yaml"),
            "rollup_commit_hash: def456\n",
        )
        .unwrap();

        let versions = load_versions_replacing_last_with_local_head(temp.path()).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(
            versions[0].binary_source,
            BinarySource::RemoteCommit("abc123".to_string())
        );
        assert_eq!(versions[0].config_commit.as_deref(), Some("abc123"));
        assert_eq!(versions[0].start_height, None);
        assert_eq!(versions[0].stop_height, Some(510));
        assert_eq!(versions[1].binary_source, BinarySource::LocalHead);
        assert_eq!(versions[1].config_commit, None);
        assert_eq!(versions[1].start_height, Some(511));
        assert_eq!(versions[1].stop_height, None);
    }
}
