# Native package recipes

These recipes package the native `parrot` binary with its production local
composition of `new`, resume, `serve`, and `web`, its unchanged locked frontend
build, its configuration and prompts, its skills with their retained notices, its
protocols, and the retained Linux PTY helper. Artifact checks and local executable
checks are separate. A successful artifact smoke alone is not security,
full-application, or release acceptance.

## Named outputs

| Actual runner | Output | Binary target |
| --- | --- | --- |
| x86_64 Linux and NixOS | `packages.x86_64-linux.parrot-linux` | native glibc on Nix |
| x86_64 Linux | `packages.x86_64-linux.parrot-musl` | standalone static musl |
| Apple Silicon macOS | `packages.aarch64-darwin.parrot-darwin` | native arm64 Darwin |

Build from a Git worktree with the relevant source and configuration committed
first:

```sh
nix build .#parrot-linux
nix build .#parrot-musl
# On a REAL Apple Silicon macOS runner only:
nix build .#parrot-darwin
```

The root flake registers these expressions through the shared integration owner.
Initial dependency acquisition may use the network. Once the locked npm and Cargo
dependencies, the compiler, and the platform closure are available, package build
and smoke phases use no live provider, credentials, or account. `nix --offline`
alone is not a network sandbox for fixed-output builders. Do not represent initial
dependency fetching as an offline verification run.

The source filter excludes worktree `.tmp`, caches, test and reference output
trees, `node_modules`, and untracked `web/dist`. The frontend is rebuilt from its
retained sources and package lock. Cargo uses its unchanged lock, local reviewed
patches, `--offline --locked`, and `CARGO_NET_OFFLINE=true`. General P05 and
security tests are not run by the packaging derivation. Those remain deferred to
P15.

## Install, artifact smoke, and closure

A Nix profile install can select the corresponding output. A standalone musl
installation must retain both `bin/` and `share/` together. The Linux helper must
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
the actual `--help` and `--version` output, and `sessions` execution from an empty
root. It also checks the architecture, and for musl it checks that both
executables lack an ELF interpreter and NEEDED entries. It explicitly does not
check local application startup and remains the Nix install-check mode.

Without that flag, the script also runs the repository's `local_production`
integration fixture against the **absolute package binary**, using
`PARROT_TEST_BINARY`. It exercises real local creation and resume,
provider-failure cleanup, authorized image upload, UDS serve RPC, embedded web
asset and session handling, and shutdown. Only the loopback HTTP provider is
synthetic. The executable uses the production capability bundle. The fixture uses
private HOME, XDG, and workspace roots, not personal credentials or live accounts.
Any fixture or build failure fails the full smoke. There is no successful skip.
This is focused executable coverage, not the general security campaign or release
acceptance.

Run full smoke from a matching committed source worktree with the pinned Rust
toolchain and Cargo, the protobuf compiler, and the locked Cargo dependencies
already available offline. The workspace also needs its genuine locked frontend
build in `web/dist`, because Cargo compiles the fixture's host dependencies.
`nix develop` provides the build tools. Prepare dependency caches and frontend
output before requesting offline verification. The fixture runs with
`cargo test --offline --locked` and does not fetch missing dependencies. Existing
`CARGO_HOME` and `CARGO_TARGET_DIR` settings are respected.

```sh
scripts/package-smoke --prefix "$(nix build --no-link --print-out-paths .#parrot-linux)" \
  --target x86_64-linux
```

Artifact roots and raw CLI outputs are retained at the printed private location
(or `PARROT_PACKAGE_SMOKE_DIR`). Full-mode fixture output is additionally recorded
in `local-production.log`. The fixture reports retained child-log paths on
failure.

Audit the actual runtime closure and linked libraries after the build. Development
SDKs and test runners do not belong in the runtime output or closure. Build tools
and Cargo metadata are not runtime linked-library evidence. Retained skill `.py` source files are inert
resources. They are not a Python runtime dependency, and this recipe never
executes them. Executing the packaged `queue` skill requires Python 3 in the
agent's command environment. The native package does not supply the interpreter.

## Native Apple Silicon recipe

On an actual `Darwin/arm64` runner:

```sh
nix build .#parrot-darwin
scripts/package-smoke --prefix "$(nix build --no-link --print-out-paths .#parrot-darwin)" \
  --target aarch64-darwin --artifact-only
otool -L "$(nix build --no-link --print-out-paths .#parrot-darwin)/bin/parrot"
```

The smoke command rejects other platforms with exit code 2 and
`blocked_external`. Linux evaluation or a cross-compile is not native macOS
acceptance. Real ARM64 native package, PTY, and sandbox verification remains an
external release prerequisite under A004. No remote machine is provisioned by
these recipes.

## Attribution and limits

`share/parrot/licenses/cargo/cargo-notices.json` records resolved Cargo package
license expressions and copied available notice texts. The separate `web` tree
records installed locked frontend package notices. Build and target-only
dependencies may appear in those inventories. Skill notices remain alongside
their resources. The Rust distribution's copyright, MIT, and Apache texts and the
Unicode data license are retained separately under `licenses/rust/` and
`licenses/UNICODE-LICENSE.txt`. Some upstream published archives omit notice
files. Their license expressions remain recorded, and no replacement texts are
invented. No legal clearance claim is made. The smoke auditor requires Bash, GNU
coreutils and findutils, and either Linux `readelf` or native Darwin `otool`, as
provided by the Nix build environment. `SOURCE-ATTRIBUTION.md` preserves the
pinned repository attribution. The pinned source has no root project license
grant. `UNSPECIFIED` entries must not be misrepresented as redistribution
permission. Review unresolved terms before any release or publication. This task
does not authorize publication.

Earlier security and procedure failures remain historical audit facts. In
particular, T36 unsafe witness `07711a1` was user-cancelled and backend-rejected.
The new production implementation and its focused executable fixture do not
retroactively accept that witness. Candidate workspace data is not a permission
grant. Python verification wrappers remain prohibited and NOT_RUN. General P05
verification remains last P15. Package artifacts and focused local smoke do not
override strict T75 rejection or those limits.
