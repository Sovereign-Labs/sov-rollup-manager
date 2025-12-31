# upgrade-simulator
This is a testing framework that allows running through upgrades of a real rollup binary using sov-rollup-manager.

It takes as input a TestCase, which takes a git repo of a rollup and a set of commit hashes corresponding to each version; builds binaries for every version of the rollup, if not already cached; and then runs the rollup manager using a generated upgrade config corresponding to these versions.

## Test case setup
Each test case goes into a subfolder inside `test-cases/`. The subfolder name is the test case name, and can be selected when running the simulator with the `--test-case` argument.

Inside the test case, the simulator expects to see the following structure:
 * `genesis.json`, which will be passed to the rollup as the genesis path
 * `test_case.toml` containing the test case definitions. `example-testcase` provides an example configuration with inline comments documenting the file format.
 * A series of folders named `v0`, `v1` etc., as many as there are versions defined in the test case, each containing a file named `config.toml` which will be passed to the rollup binary as its config file

### Storage path
The test framework assumes the following about the config:
1. The storage path and the MockDA storage are both using relative paths - this allows the simulator to clean up after each run. There is no protection against the rollup polluting paths outside of its data directory!
2. MockDA data is stored outside of the rollup `storage.path` directory. This allows the simulator to simulate resyncing a fresh node over the DA data, by wiping just the state directory and re-running the test a second time.

Additionally, all version configs must point to the same storage path. Currently, all versions must also expose their HTTP API on the same port, as only the first version's config is parsed to read it.

## Test strategy
When running a test case, the simulator will:
 1. Check out the rollup repository, and build binaries of the rollup at each commit corresponding to each version.
 2. Spawn a `sov-rollup-manager` process with a generated configuration corresponding to the versions defined in `test_case.toml`
 3. Spawn a soak test binary corresponding to each version. Currently, this requires the soak test to be defined in the same repo as it must import the rollup's `Runtime` type; as such they are built during step 1 as well. This works out of the box with forks of `rollup-starter`.
 4. Run through every version defined in the test case. If at any point either the soak test or the rollup exits with an error, the test case fails.
 5. Wipe the rollup node's data directory, *without* wiping the entire test run directory. With a default `MockDa` config, this allows DA data to persist.
 6. Spawn `sov-rollup-manager` again, resyncing the rollup over the data generated in the previous step.
 7. Once the last version is reached, optionally run the rollup for some additional blocks, spawning a corresponding soak test binary again - to verify that the rollup continues accepting transactions after a full resync.
