Ported from [Parrot .NET's AGENTS.md](https://github.com/asdacap/parrotdotnet/blob/28e206c43829aa366e31bdeb0dd52711e3108d88/AGENTS.md)
at commit `28e206c43829aa366e31bdeb0dd52711e3108d88`, with Rust terminology,
lifecycle examples, build gates and source paths adapted for this repository.
See also [Design philosophy](docs/design-philosophy.md).

## Guideline

- No comment unless necessary
- Traits should have comments at the top level and on each method to document the behavior except if the name of the
method is already obvious.
- Traits and implementations are fine and in fact preferable.
- If a domain type has a trait, always use the trait rather than the implementation. Do not downcast to implementation.
- No two domain traits per implementation please.
- Dependency injection.
- Prefer rich domain objects rather than anemic domain models.
  - For example, if something is agent session specific, create an instance per agent session and store in the same
  scope as the agent session.
- No .NET-style events.
- No subclass unless necessary; prefer composition in Rust.
- No subtrait unless necessary.
- NEVER turn off lint rules repo wide or crate wide. Exception is test crates can have lint rules disabled AFTER user
  explicit permission.
- Suffix the type name properly. If a type is an event, suffix it with Event.
- Do not make a method/function just to construct an object, unless that method is on that type. Dont do create_event(something). Do Event::create(something).
- Type names are nouns and declarative. Method names are imperative. Its an action (unless its the type constructor). So
  method names MUST have action please, come on man...
- Reduce coupling, as in the public method count.
- Dont share or deduplicate unnecessarily especially at the expense of coupling and code indirection
- Prefer async workflow over manual state machine.
- No optional parameter.
- Robust lazy validation. Try not to validate early, rather make later validation handling robust.
- Lazy materialization. This follows the same spirit as lazy validation. Do not pre-optimize state, only compile them
right before the information is needed.
- A user session (and all its component including all agent sessions) data is scoped within that user session. Do not
architect the code in a way that conflict with this rules, as in keep user session data in its own directory unless
otherwise said.
- Tools or any permission check should check the security profiles rather than specific directory.
- Do not make unnecessary change in prod for test.
- No method overload. Use different method name instead.
- Runtime downcasting or type checks tend to be an antipattern. Rather than downcasting T1 to T2, make a T3 or expand T1 to include T2's trait.
- Configuration are like the atomic representation of code. Making more code just to assert config exactly are
duplicated code, so dont make unit test like that. This is not the same as making a unit test that test behavior of
config value.
- Every interaction with the agent, as in the prompt MUST be templated and made configurable via predefined_config for
easy review and modification.
- DO NOT modify generated predefined_config.yaml or existing values in assets/config/predefined_config.*.yaml without user authorization. You may add new keys, but do not modify existing
keys. These are critical files that should be human modified only. If you do have to modify an existing prompt, do not change its wording or rephrase it.
- Consider very long session, which mean long history. Do not do a linear scan unless necessary.

## Async lifecycle

- Rather than something like:
```rust
let mut component = Something::new();
component.start();
component.stop();
```

Just do.

```rust
let mut component = Something::new();
component.run(cancellation).await?;
```
- In fact, there should probably be a single top level `run` that is the parent of all other runs.

## SOLID preferences.

- Single Responsibility: Eh...
- Open Close: Yes. Something tend to be wrong when this is not the case.
- Liskov Substitution: Or just straight up no inheritance. Just compose.
- Interface Segregation: Eh...
- Dependency Inversion: Decent...

## Environment

Everything runs inside the flake's dev shell, which supplies the pinned Rust
toolchain, Clippy, rustfmt, C toolchain, Node/npm and protobuf tooling.

```sh
nix develop
```

`nix run . -- <args>` works without the dev shell. The package recipes are in
`nix/packages/default.nix`; Rust dependencies are locked in `Cargo.lock`, and
frontend dependencies in `web/package-lock.json`. See the
[installation guide](docs/operator/installation.md) for package builds and
platform requirements.

**`git add` a new file before `nix build`/`nix run`.** A flake sees only
git-tracked files, so an untracked `.rs` is dropped from the build and can surface
as a missing module or type. Dev-shell Cargo commands read the working tree
directly, so a green `cargo build` can still fail under Nix until the file is
tracked.

## Gates

Run inside `nix develop`:

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
nix flake check
```

The [CI workflow](.github/workflows/check.yml) also defines frontend, contract,
native-platform and package-smoke gates. The workspace denies Rust warnings and
Clippy's `all` lint group; this is native Rust, not a .NET Native AOT build.

## The rules that get broken most often

- **Do not deviate from the plan.** If the plan is wrong, stop and say so rather
  than improvising a fix.
- **Do not port a component the map does not name.** Fix the map first.
- **Never turn a rule off.** Not inline, not per-file, not repository-wide.
  Lint-suppression attributes such as `#[allow(...)]` and `#[expect(...)]` are not
  permitted, and neither is weakening the lint configuration in `Cargo.toml`.
  Change the code.
  A rule firing on code you believe is correct usually means the code is more
  public, more mutable, or more general than it needs to be.
- **No runtime reflection or reflection-based JSON.** Use statically typed or
  generated serialization.
- **No stubs that return plausible values.** An unimplemented path returns an error or panics explicitly.
- **Matching an id to change behaviour is an antipattern.** Put it on the
  abstraction.
- **Prefer a trait over a config flag**, and do not add a trait with a
  single implementation just to enable a mock.
- **A failing ported test is skipped and reported**, never weakened or deleted.

## Commits

A commit message for a migrated component names the upstream .NET components or
source paths it absorbed. Check `git status` for unexpected changes before
committing; if there are any, do not commit.

When the user says “commit”, also clean generated build, test, and dependency
artifacts from this worktree before committing. Preserve user files and unrelated
changes; do not use broad cleanup commands.

## Analogy

A good software is like a nice garden. There are distinct clear trees, each tree can be of different shape,
and size, but its clearly itself, and there are roads to clearly move around the garden. The purpose of the garden
is the tree, not the road, but without the road, its hard to plant tree.

## Nomenclature

- A user-conversation-turn is from first message to final message.
- A tool-call-cycle is the provider request -> response.
