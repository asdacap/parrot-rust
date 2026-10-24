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
