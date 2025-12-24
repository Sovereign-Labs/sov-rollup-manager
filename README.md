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
In order to detect when a rollup has reached the stop height, 
