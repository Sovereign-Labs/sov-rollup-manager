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

Each version runs in sequence. Sovereign SDK rollups take `--stop-at-height` and `--start-at-height` arguments, which cause the sequencer to stop processing after producing the given height, and to validate that the current height is above the start one on startup. The rollup manager thinly wraps the rollup node binary, except for when the rollup reaches the specified stop height; in this case the manager runs the configured migration (if any), and then launches the next version configured.

## Rollup HTTP config
In order to detect when a rollup has reached the stop height, the manager queries the local node's HTTP API. This means the node must provide:
 * A `runner.http_config.bind_port` config value in the file at `config_path`

Additionally, for completeness, the manager relies on:
 * The rollup being available to being queried over `localhost:{bind_port}`
 * The `modules/chain-state/state/current-heights/` API endpoint
Both of those should be available by default in any standard Sovereign SDK-based rollup. The ChainState endpoint is exposed by a core module that is always included in rollups that use the Sovereign SDK's module system.

## Process management
Other than handling version upgrades at specified stop heights, the rollup manager doesn't perform process management: this is meant to be handled by an external system, e.g. systemd. Specifically:
 * If a rollup exits with a non-zero exit code, the manager will exit with the same exit code. (TODO)
 * If a rollup exits with a 0 exit code but not at an expected stop height, the manager will exit with a non-zero exit code. Avoid sending `SIGTERM` directly to the child rollup process.
 * The rollup manager forwards `SIGTERM` and `SIGQUIT` to the rollup process. (TODO)
