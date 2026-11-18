# Run environment-dependent tests

This standalone Cargo workspace owns executable, Node Connect client, and process-crash tests. Root `cargo test --workspace` does not include these tests. Ordinary host builds still need generated web assets.

Run these commands from the repository root inside `nix develop`:

```sh
export CARGO_TARGET_DIR="$(git rev-parse --path-format=absolute --git-common-dir)/../.tmp/environment-target"
export CARGO_BUILD_JOBS="$(getconf _NPROCESSORS_ONLN)"
export RUST_TEST_THREADS="$CARGO_BUILD_JOBS"
export TMPDIR=/mnt/workspace/tmp
npm --prefix web ci --no-audit --no-fund
npm --prefix web run build
cargo build --locked -p parrot-cli --bin parrot
export PARROT_TEST_BINARY="$CARGO_TARGET_DIR/debug/parrot"
cargo build --manifest-path tests/environment/Cargo.toml --locked --example t17-crash-probe
export T17_CRASH_PROBE_BINARY="$CARGO_TARGET_DIR/debug/examples/t17-crash-probe"
export PARROT_T64_BROWSER_MODULES="$PWD/web/node_modules"
cargo test --manifest-path tests/environment/Cargo.toml --locked
```

Use a short local directory for `TMPDIR`. On other systems, replace `/mnt/workspace/tmp` with an available short local path. Unix socket names must fit the platform's path limit.

`local_production` requires `PARROT_TEST_BINARY` to name the built CLI. Package smoke sets this to the packaged binary. The `browser` target requires Node, generated TypeScript, and the installed module tree in `PARROT_T64_BROWSER_MODULES`. The `crash_recovery` target requires the example binary in `T17_CRASH_PROBE_BINARY`. Missing fixtures fail their tests rather than skip assertions.

Run one target with `--test local_production`, `--test browser`, or `--test crash_recovery`. CI runs this crate explicitly after the ordinary workspace tests.
