# Sov Rollup Manager
A wrapper binary that manages Sovereign SDK rollup nodes, automatically upgrading the rollup binary and running state migrations across hard fork upgrades.

## Functionality
The rollup manager takes a JSON config that looks like this:
```json
[
    {
        "rollup_binary": "path/to/v0/rollup",
        "config_path": "path/to/config.toml",
        "stop_height": 100
    },
    {
        "rollup_binary": "path/to/v1/rollup",
        "config_path": "path/to/config.toml",
        "start_height": 101,
        "stop_height": 220
    },
    {
        "rollup_binary": "path/to/v2/rollup",
        "config_path": "path/to/upgraded/config.toml",
        "migration_path": "path/to/state_migration_script.sh",
        "start_height": 221
    }
]
```

Each version runs in sequence. The rollup manager thinly wraps the rollup node binary, except for when the rollup reaches the specified stop height; in this case the manager runs the configured migration (if any), and then launches the next version configured.

## Checkpoint file
The rollup manager uses a checkpoint file to save the currently running version across restarts. *If and only if* the node state is wiped (triggering a full resync), then the checkpoint file should be deleted as well. Trying to run an empty state with a non-empty checkpoint or vice-versa will crash on startup if the versions don't match, but is not otherwise dangerous.

### Rollup paths
The checkpoint file is contains the path to the current version's binary for sanity checking. As such, when preparing for an upgrade, it is recommended to place the new version's binary will in a new location, rather than replacing the current latest version and moving the currently running version.

If the rollup binary is nevertheless moved or renamed for any reason, the checkpoint file is human-readable JSON and can be manually edited to point to the new location.

## Example usage
A minimal (and typical) usage looks like this:
```shell
rollup-manager -c path/to/config.json -f path/to/checkpoint.json
```
Where config.json is canonical to the chain being ran, and checkpoint.json is a local persistent file of your choice that the rollup process has write permissions to.

## Node communication
The rollup manager mostly transparently wraps the rollup process, and should work out of the box with any Sovereign SDK based rollup using normal configuration. This section documents in detail the manager <-> rollup communication in case this is of interest or to assist with particularly unusual configurations.

### Rollup HTTP config
In order to detect when a rollup has reached the stop height, the manager queries the local node's HTTP API. This means the node must provide:
 * A `runner.http_config.bind_port` config value in the TOML file at `config_path`

Additionally, for completeness, the manager relies on:
 * The rollup being available to being queried over `localhost:{bind_port}`
 * The `modules/chain-state/state/current-heights/` API endpoint
Both of those should be available by default in any standard Sovereign SDK-based rollup. The ChainState endpoint is exposed by a core module that is always included in rollups that use the Sovereign SDK's module system.

### Process management
Other than handling version upgrades at specified stop heights, the rollup manager doesn't perform process management: this is meant to be handled by an external system, e.g. systemd. Specifically:
 * If a rollup exits with a non-zero exit code, the manager will exit with the same exit code.
 * If a rollup exits with a 0 exit code but not at an expected stop height, the manager will exit with a non-zero exit code. Avoid sending `SIGTERM` directly to the child rollup process - use the rollup manager process to control the rollup.
 * The rollup manager forwards `SIGTERM` and `SIGQUIT` to the rollup process. Other signals are currently swallowed.
