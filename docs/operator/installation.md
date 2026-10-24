# Installation and bounded offline operation

See [state and rollback](state-and-rollback.md) **before opening existing state**.
These are experimental native artifacts, not a released application. Local
session creation (`/new`), local resume requiring a new service, `serve` and `web`
remain unavailable/fail-closed. Installing dependencies or passing an artifact
check does not supply the missing production capability bundle.

## Build prerequisites

Use a real Git worktree with relevant source/config committed and the locked
`flake.lock`, `Cargo.lock` and frontend lock. Nix supplies Rust1.85, the C toolchain,
Node/npm and protobuf tooling. Ordinary initial dependency acquisition may need
the network; **do not call that acquisition offline**. The commands below assume
its dependency closure is already available. `nix --offline` is not itself a
network sandbox for fixed-output builders.

From the committed checkout on x86_64 Linux/NixOS:

```sh
REF="git+file:$PWD?rev=$(git rev-parse HEAD)"
nix build --offline --option eval-cache false --no-write-lock-file \
  --max-jobs 8 --cores 8 --no-link --print-out-paths \
  "$REF#packages.x86_64-linux.parrot-linux" \
  "$REF#packages.x86_64-linux.parrot-musl"
```

The printed paths identify real package prefixes. Select the target you intend
to install (its `share/parrot/target` records the target); do not substitute a
fabricated prefix after a failed build. For example, obtain a Linux prefix and install into a **new, private**
profile rather than silently modifying the user's default profile:

```sh
prefix="$(nix build --offline --no-write-lock-file --no-link --print-out-paths \
  "$REF#packages.x86_64-linux.parrot-linux")"
profile="$PWD/.tmp/operator-linux-profile"
nix profile add --profile "$profile" "$prefix"
bash scripts/package-smoke --prefix "$profile" --target x86_64-linux --artifact-only
```

Use an unused profile path; keep an older known-good profile/package available
rather than deleting it. This is not a universal installer or an
operating-system service setup. Even help/version initialize
diagnostic logs; prefer the smoke script's synthetic HOME/XDG/TMP roots over
pointing an experimental binary at personal state. The script checks help,
version, empty-root `sessions`, packaged resources/notices and architecture.

For musl use `parrot-musl` and `--target x86_64-linux-musl`. A standalone
installation must keep the complete `bin/` and `share/` trees together, including
`bin/parrot-pty-attach`, resource hashes and notices. Copying only `parrot` is not
a supported package layout.

Without `--artifact-only`, the **required** smoke retains artifact observations
but returns **exit3 / BLOCKED_LOCAL_STARTUP**. Do not replace a release gate with
artifact-only mode or relabel this required failure as a successful skip. No local
provider or fake production backend is started by the smoke.

## Native runtime / sandbox dependencies

- Linux shell isolation requires trusted-system-path **Bubblewrap (`bwrap`)** and
  usable unprivileged user namespaces. Linux PTY launch needs the retained helper
  beside the CLI. The package's static musl property does not install Bubblewrap,
  grant permissions or prove a working production sandbox.
- macOS pipe isolation uses **`/usr/bin/sandbox-exec` (Seatbelt)** at a trusted,
  non-writable system path; it is compatibility isolation, not a promised robust
  hostile-command boundary. PTY shell mode is unsupported on macOS. Missing
  sandbox support is not permission to run unconfined. The incomplete local
  production security bundle remains a separate blocker on every platform.
- A source build uses the locked Nix toolchain. Native macOS requires a real
  Apple Silicon macOS runner with its Darwin SDK/toolchain closure. The original
  source guide lists Xcode Command Line Tools, but this native dependency closure
  has not been validated here. Its .NET SDK requirement belongs to the original
  development oracle, not the Rust runtime. Linux evaluation/cross-compilation
  cannot establish that platform's behavior.

The Apple Silicon recipe is **not executed natively in current evidence**:

```sh
# On a REAL Darwin/arm64 runner only, after its dependency closure exists:
nix build --offline --no-write-lock-file --no-link --print-out-paths \
  "$REF#packages.aarch64-darwin.parrot-darwin"
```

A Darwin smoke on Linux returns exit2 / `blocked_external`; it is not a Mac
test pass. Intel macOS, Windows and Linux ARM64 are not supported package targets.

## Interfaces versus available operation

`parrot help`, `parrot version` (or `--version`) and `parrot sessions` retain their
interfaces. Top-level `sessions` reads metadata from the **new state root**; it is
not a merged legacy-session listing or evidence that local resume succeeded.
`parrot chat --basic` selects the minimal renderer, not a security bypass.
See [enhanced CLI Markdown output](markdown.md) for the enhanced renderer's syntax,
highlighting and streaming behavior.

`parrot chat --connect <address> [--token-file <owner-only-file>]` is an explicit
experimental attach interface for a separately authorized compatible server, not
a way to start the missing local service. Addresses include `unix:/path`, HTTP
and HTTPS. TCP uses an owner-only token file; plaintext non-loopback listening
also requires `--unsafe-allow-external`. HTTPS serving additionally requires
`--tls-cert-file <PEM-chain>` and `--tls-key-file <unencrypted-PEM-key>`; see
[HTTPS provisioning](../../README.md#https-certificates-for-serve).
No example here authorizes a live provider, remote server/account, OAuth or S3
action. Authentication/model-list commands may access credentials or a network and
are **not offline smoke checks**.
Do not infer available local operation merely because help lists a command.

## Attribution and retention

Keep `share/parrot/licenses/`, skill notices and `THIRD_PARTY_NOTICES.md` with the
package; see [third-party notices](../../THIRD_PARTY_NOTICES.md) and the
[package recipes](../../packaging/README.md). Some workspace license metadata is
UNSPECIFIED and published dependency notices are incomplete. Legal clearance is
not established by installation.
