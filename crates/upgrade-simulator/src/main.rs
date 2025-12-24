use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info};

use upgrade_simulator::{run_test_case, RollupBuilder, TestCase, VersionSpec};

#[derive(Parser)]
#[command(name = "upgrade-simulator")]
#[command(about = "Simulate and test rollup upgrades using sov-rollup-manager")]
struct Cli {
    /// Directory for caching repository and binaries
    #[arg(long, default_value = "rollup-build-cache")]
    cache_dir: PathBuf,

    /// Root directory containing test case subdirectories
    #[arg(long, default_value = "test-cases")]
    test_root: PathBuf,

    /// Name of the test case to run (runs all if not specified)
    #[arg(short, long)]
    test_case: Option<String>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        cache_dir = %cli.cache_dir.display(),
        test_root = %cli.test_root.display(),
        "Upgrade simulator starting"
    );

    let builder = RollupBuilder::new(cli.cache_dir);

    // Define test cases in code for now
    let test_cases = vec![TestCase {
        name: "starter-trivial-test".to_string(),
        versions: vec![
            VersionSpec {
                commit: "db39657a04295d659712b6384b9950c45922d5e4".into(),
                start_height: None,
                stop_height: Some(10),
            },
            VersionSpec {
                commit: "db39657a04295d659712b6384b9950c45922d5e4".into(),
                start_height: Some(11),
                stop_height: Some(15),
            },
        ],
    }];

    // Filter to requested test case if specified
    let cases_to_run: Vec<_> = if let Some(ref name) = cli.test_case {
        test_cases
            .into_iter()
            .filter(|tc| tc.name == *name)
            .collect()
    } else {
        test_cases
    };

    if cases_to_run.is_empty() {
        error!(requested = ?cli.test_case, "No matching test cases found");
        return ExitCode::FAILURE;
    }

    for test_case in &cases_to_run {
        if let Err(e) = run_test_case(&builder, &cli.test_root, test_case) {
            error!(name = %test_case.name, error = %e, "Test case failed");
            return ExitCode::FAILURE;
        }
    }

    info!("All test cases passed");
    ExitCode::SUCCESS
}
