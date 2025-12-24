use std::path::PathBuf;
use std::process::ExitCode;
use std::{fs, io};

use clap::Parser;
use tracing::{error, info};

use upgrade_simulator::{load_test_case, run_test_case, TestCase};

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

    /// Test cases to run (comma or space separated).
    #[arg(short, long, value_delimiter = ',', num_args = 1.., conflicts_with = "all")]
    test_cases: Option<Vec<String>>,

    /// Run all test cases found in test_root.
    #[arg(long, conflicts_with = "test_cases")]
    all: bool,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        cache_dir = %cli.cache_dir.display(),
        test_root = %cli.test_root.display(),
        "Upgrade simulator starting"
    );

    // Validate that either --all or --test-cases is specified
    if !cli.all && cli.test_cases.is_none() {
        error!("No test cases specified! List test case names with `--test-cases` or specify `--all` to run all available tests.");
        return ExitCode::FAILURE;
    }

    // Load test cases from test_root directory
    let filter = if cli.all { None } else { cli.test_cases.as_deref() };
    let test_cases = match load_test_cases(&cli.test_root, filter) {
        Ok(cases) => cases,
        Err(e) => {
            error!(error = %e, "Failed to load test cases");
            return ExitCode::FAILURE;
        }
    };

    if test_cases.is_empty() {
        if cli.all {
            error!("No test cases found in {}", cli.test_root.display());
        } else {
            error!(requested = ?cli.test_cases, "No matching test cases found");
        }
        return ExitCode::FAILURE;
    }

    info!(count = test_cases.len(), "Loaded test cases");

    for test_case in &test_cases {
        if let Err(e) = run_test_case(&cli.cache_dir, &cli.test_root, test_case) {
            error!(name = %test_case.name, error = %e, "Test case failed");
            return ExitCode::FAILURE;
        }
    }

    info!("All test cases passed");
    ExitCode::SUCCESS
}

/// Load test cases from the test root directory.
///
/// If `names` is provided, load those specific test cases in the given order.
/// Otherwise, load all test cases found in subdirectories (sorted by name).
fn load_test_cases(
    test_root: &PathBuf,
    names: Option<&[String]>,
) -> Result<Vec<TestCase>, io::Error> {
    let mut test_cases = Vec::new();

    if let Some(names) = names {
        // Load specific test cases in the order specified
        for name in names {
            let test_case_dir = test_root.join(name);
            match load_test_case(&test_case_dir) {
                Ok(tc) => test_cases.push(tc),
                Err(e) => {
                    error!(name, error = %e, "Failed to load test case");
                }
            }
        }
    } else {
        // Scan for all test case directories
        for entry in fs::read_dir(test_root)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() && path.join("test_case.toml").exists() {
                match load_test_case(&path) {
                    Ok(tc) => {
                        info!(name = %tc.name, "Found test case");
                        test_cases.push(tc);
                    }
                    Err(e) => {
                        error!(path = %path.display(), error = %e, "Failed to load test case");
                    }
                }
            }
        }

        // Sort by name for deterministic ordering (only when discovering all)
        test_cases.sort_by(|a, b| a.name.cmp(&b.name));
    }

    Ok(test_cases)
}
