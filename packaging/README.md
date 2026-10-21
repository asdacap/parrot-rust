# Native package recipes

These recipes package the native `parrot` binary with its production local
`new`/resume/`serve`/`web` composition, unchanged locked frontend build,
configuration/prompts, skills and their retained notices, protocols, and retained
Linux PTY helper. Artifact checks and local executable checks are separate; a
successful artifact smoke alone is not security, full-application, or release
acceptance.

## Named outputs

| Actual runner | Output | Binary target |
| --- | --- | --- |
| x86_64 Linux/NixOS | `packages.x86_64-linux.parrot-linux` | native glibc/Nix |
| x86_64 Linux | `packages.x86_64-linux.parrot-musl` | standalone static musl |
| Apple Silicon macOS | `packages.aarch64-darwin.parrot-darwin` | native arm64 Darwin |

Build from a Git worktree with the relevant source/configuration committed first:

```sh
nix build .#parrot-linux
nix build .#parrot-musl
# On a REAL Apple Silicon macOS runner only:
nix build .#parrot-darwin
```

The root flake registers these expressions through the shared integration owner.
Initial dependency acquisition may use the network. After the locked npm/Cargo,
compiler and platform closure exists, package build and smoke phases use no live
provider, credentials or account. `nix --offline` alone is not a network sandbox
for fixed-output builders; do not represent initial dependency fetching as an
offline verification run.

The source filter excludes worktree `.tmp`, caches, test/reference output trees,
`node_modules` and untracked `web/dist`. The frontend is rebuilt from its retained
sources and package lock. Cargo uses its unchanged lock, local reviewed patches,
`--offline --locked` and `CARGO_NET_OFFLINE=true`. General P05/security tests are
not run by the packaging derivation: those remain deferred to P15.

## Install, artifact smoke, and closure

A Nix profile install can select the corresponding output. A standalone musl
installation must retain both `bin/` and `share/` together; the Linux helper must
remain alongside `parrot`. Do not copy only the CLI binary and discard notices or
resource metadata.

```sh
scripts/package-smoke --prefix "$(nix build --no-link --print-out-paths .#parrot-linux)" \
  --target x86_64-linux --artifact-only
scripts/package-smoke --prefix "$(nix build --no-link --print-out-paths .#parrot-musl)" \
  --target x86_64-linux-musl --artifact-only
nix path-info --recursive "$(nix build --no-link --print-out-paths .#parrot-linux)"
```

`--artifact-only` checks resources and their hashes, retained license inventories,
actual `--help`, `--version`, and empty-root `sessions` execution, architecture,
and (for musl) absence of ELF interpreter/NEEDED entries in BOTH executables.
It explicitly does not check local application startup and remains the Nix
install-check mode.

Without that flag, the script also runs the repository's `local_production`
integration fixture against the **absolute package binary**, using
`PARROT_TEST_BINARY`. It exercises real local create/resume, provider-failure
cleanup, authorized image upload, UDS serve RPC and shutdown, and embedded web
assets/session handling and shutdown. Only the loopback HTTP provider is
synthetic; the executable uses the production capability bundle. The fixture
uses private HOME/XDG/workspace roots, not personal credentials or live accounts.
Any fixture/build failure fails the full smoke; there is no successful skip.
This is focused executable coverage, not the general security campaign or release
acceptance.

Run full smoke from a matching committed source worktree with the pinned Rust
Cargo/toolchain, protobuf compiler, and the locked Cargo dependencies already
available offline. The workspace also needs its genuine locked frontend build in
`web/dist`, because Cargo compiles the fixture's host dependencies. `nix develop`
provides the build tools; prepare dependency caches and frontend output before
requesting offline verification. The fixture runs with
`cargo test --offline --locked` and does not fetch missing dependencies. Existing
`CARGO_HOME` and `CARGO_TARGET_DIR` settings are respected.

```sh
scripts/package-smoke --prefix "$(nix build --no-link --print-out-paths .#parrot-linux)" \
  --target x86_64-linux
```

Artifact roots and raw CLI outputs are retained at the printed private location
(or `PARROT_PACKAGE_SMOKE_DIR`); full-mode fixture output is additionally recorded
in `local-production.log`. The fixture reports retained child-log paths on failure.

Audit the actual runtime closure and linked libraries after the build. No .NET
assembly/runtime, source-reference runner or development SDK is permitted in the
native runtime output/closure. Build tools and Cargo metadata are not runtime
linked-library evidence. Retained skill `.py` source files are inert resources,
not a Python runtime dependency and are never executed by this recipe.

## Native Apple Silicon recipe

On an actual `Darwin/arm64` runner:

```sh
nix build .#parrot-darwin
scripts/package-smoke --prefix "$(nix build --no-link --print-out-paths .#parrot-darwin)" \
  --target aarch64-darwin --artifact-only
otool -L "$(nix build --no-link --print-out-paths .#parrot-darwin)/bin/parrot"
```

The smoke command rejects other platforms with exit **2 / blocked_external**.
Linux evaluation or a cross-compile is not native macOS acceptance. Real ARM64
native package, PTY and sandbox verification remains an external release
prerequisite under A004. No remote machine is provisioned by these recipes.

## Attribution and limits

`share/parrot/licenses/cargo/cargo-notices.json` records resolved Cargo package
license expressions and copied available notice texts; the separate `web` tree
records installed locked frontend package notices. Build/target-only dependencies
may appear in those inventories. Skill notices remain alongside their resources.
The Rust distribution's copyright/MIT/Apache texts and the Unicode data license
are retained separately under `licenses/rust/` and `licenses/UNICODE-LICENSE.txt`.
Some upstream published archives omit notice files; their license expressions
remain recorded, not invented replacement texts or a legal clearance claim.
The smoke auditor requires Bash, GNU coreutils/findutils and Linux `readelf` or
native Darwin `otool`, as provided by the Nix build environment.
`SOURCE-ATTRIBUTION.md` preserves the pinned repository attribution. The pinned
source has no root project license grant: `UNSPECIFIED` entries must not be
misrepresented as redistribution permission. Review unresolved terms before any
release/publication; this task does not authorize publication.

Earlier security/procedure failures remain historical audit facts. In particular,
T36 unsafe witness `07711a1` was user-cancelled and backend-rejected; the new
production implementation and its focused executable fixture do not retroactively
accept that witness. Candidate workspace data is not a permission grant. Python
verification wrappers remain prohibited/NOT_RUN. General P05 verification remains
last P15; package artifacts and focused local smoke do not override strict T75
rejection or those limits.
