# Parrot — experimental native Rust port

Native port of Parrot, with behavior updates against .NET reference commit
`4eb01b9acd83cfab7ce8b18491bb311180fff987`. The executable remains `parrot`;
the retained browser frontend, protocols, prompts, skills and Linux C PTY helper
are part of the project. The .NET application is a development reference, not a
native runtime dependency.

**Not a production-ready or release-accepted replacement.** Local session
creation/resume, `serve` and `web` now use the native production service and
security/capability bundle. These paths, existing-owner attachment and explicit
remote `chat --connect` remain experimental; this is not a claim of complete
.NET behavior parity. Do not substitute an unrestricted or test/denying backend.
Templates use MiniJinja; existing Scriban templates require manual conversion.

**Before using existing state:** resuming a selected legacy session can **mutate
its old database and session directory in place**. Back up before any Rust write.
Never run .NET and Rust against the same workspace concurrently. Existing .NET
cannot observe ownership records in the new Rust root, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Design philosophy

See [Design philosophy](docs/design-philosophy.md) for the original principles
on bounded agent scope, delegation, parent attention and model-tier selection,
ported from the .NET repository. Repository contributor guidance is in
[AGENTS.md](AGENTS.md).

## Core and runtime

`parrot-core` owns the agent turn protocol and loop, with exactly three traits:

- `States`: conversation and turn effects, pending-call recovery, result recording,
  prepared requests, and completion admission.
- `Tools`: turn-bound catalogs and execution of explicitly supplied calls.
- `Provider`: model invocation and awaited stream cleanup.

Requests, responses, messages, tool definitions, errors, and tool contexts are concrete
core-owned types. Adapters implement the same interface; they do not choose their
own associated payload or context types. `Provider` accepts only the request;
attempts and request phases stay in the core loop, and input estimates stay in
runtime state. `ToolContext` selects recovery, normal, or final-phase execution
without exposing state objects or adapter-specific callbacks.

The core specifies and implements:

- Request admission/counting, interruption, final-phase catalog selection, and
  bounded tool-settlement extensions.
- Truncated tool-call detection and JSON-argument validation before recording or
  executing calls.
- The one-compaction retry guard for oversized requests, completion-policy retries,
  and request-budget resets.
- Readiness/scheduled-work decisions, recording assistant calls before execution,
  and completion ordering before closing a turn.

`parrot-runtime` implements the three traits with private adapters. Persistence,
prompt preparation, compaction, security, streaming, accounting, cancellation,
events, and session lifecycle stay outside the core. Provider representations are
translated losslessly at the interface, including raw provider extensions and
multimodal content. Runtime bindings retain the actual selected model and executable
tool snapshots; tools are never reconstructed from advertised definitions.

The core asks state for outstanding calls, passes explicit calls to tools, then
asks state to record the returned results before executing another chunk. State
restores completed calls without reinvocation or republication and owns conversation
projection, recovery progress, and synthetic image messages. Tools retain execution,
parallel-safe grouping, tool-specific budgets, and execution-local state; they do
not discover work by scanning conversation history.

The session drain serializes operations. Each adapter borrows the existing runtime
state lock only for its operation, including borrowed-resource tool execution.
Runtime state observes every streamed completion through the existing usage hook,
calibrates its token budget, then awaits durable usage recording—even if the stream
later fails. The provider adapter does not own estimates or calibration. The core
depends on neither runtime, storage, nor provider implementations.

## Compiler cache

`nix develop` provides `sccache` and sets `RUSTC_WRAPPER` to use it. The compiler
cache uses sccache's per-user default cache directory, shared across worktrees;
Cargo build artifacts remain worktree-local in `.tmp/cargo-target` unless
`CARGO_TARGET_DIR` is set. Cargo's dependency cache also uses its normal per-user
location unless `CARGO_HOME` is set externally.

Rust incremental compilations are not cacheable by sccache. To favor compiler
cache reuse across clean targets or worktrees, run `CARGO_INCREMENTAL=0 cargo build`
(or set that variable for the desired session). This does not make every Rust
compilation cacheable; for example, linking binaries is not cached. Use
`sccache --show-stats` to inspect cache hits and non-cacheable compilations.

## Running tests

Run from the repository root in `nix develop`. The native test harnesses require
an installed browser dependency tree, the crash-probe executable and the shell
from the Nix environment:

```sh
export CARGO_TARGET_DIR=/tmp/parrot-rust-test-target
npm --prefix web ci --no-audit --no-fund
npm --prefix web run build
cargo build --locked -p parrot-storage --example t17-crash-probe
export T17_CRASH_PROBE_BINARY="$CARGO_TARGET_DIR/debug/examples/t17-crash-probe"
export T57_TEST_SHELL="$(command -v bash)"
export PARROT_T64_BROWSER_MODULES="$PWD/web/node_modules"
cargo test --workspace --locked
cargo test --manifest-path tests/contracts/Cargo.toml --locked
npm --prefix web test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
nix flake check
```

Keep the target directory and feature selection consistent between runs. Atomic
compaction-artifact fixtures use `std::env::temp_dir()`; ensure `TMPDIR` is on a
local filesystem supporting atomic rename exchange, not an NFS mount. Production
artifact quarantine also requires that filesystem operation; this test setup does
not establish NFS support. Additional native-platform and package-smoke gates are
listed in [.github/workflows/check.yml](.github/workflows/check.yml).

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

### HTTPS certificates for `serve`

HTTPS serving uses the Rust-native Tonic/Rustls TLS stack with explicitly supplied
PEM files:

```sh
parrot serve --listen https://127.0.0.1:7443 \
  --token-file /run/parrot/token \
  --tls-cert-file server-chain.pem \
  --tls-key-file server-key.pem
```

Both TLS flags are required for HTTPS and rejected for HTTP or Unix-domain-socket
serving. The certificate file contains the PEM certificate chain, leaf first;
the key file contains its matching **unencrypted PEM private key**. Relative paths
resolve against the process working directory. Invalid certificates or keys fail
startup before the listening announcement. Keep the private key readable only by
the server owner.

There is no certificate discovery, generation, certificate-store lookup, PFX/PKCS12
or encrypted-key support. TLS provisioning does not read environment variables or
`appsettings*.json`. Files are loaded at startup, not through hot reload. HTTPS
still requires the owner-only bearer token file; client certificate and hostname
verification are never disabled.
