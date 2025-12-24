# upgrade-simulator
This is a testing framework that allows running through upgrades of a real rollup binary using sov-rollup-manager.

It takes as input a TestCase, which takes a git repo of a rollup and a set of commit hashes corresponding to each version; builds binaries for every version of the rollup, if not already cached; and then runs the rollup manager using a generated upgrade config corresponding to these versions.

## Test case setup
Each test case goes into a subfolder inside `test-cases/`. The subfolder name is the test case name, and can be selected when running the simulator with the `--test-case` argument.

Inside the test case, the simulator expects to see the following structure:
 * `genesis.json`, which will be passed to the rollup as the genesis path
 * `test_case.toml` containing the test case definitions - TODO
 * A series of folders named `v0`, `v1` etc., as many as there are versions defined in the test case, each containing a file named `config.toml` which will be passed to the rollup binary as its config file

### Storage path
The test framework assumes the following about the config:
1. The storage path and the MockDA storage are both using relative paths - this allows the simulator to clean up after each run.
2. MockDA data is stored outside of the rollup `storage.path` directory. This allows the simulator to simulate resyncing a fresh node over the DA data, by wiping just the state directory and re-running the test a second time.

Additionally, all version configs must point to the same storage path.
