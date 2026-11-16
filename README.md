# Parrot Rust AI coding assistant

Parrot is an AI coding assistant written in Rust.
It provides a command-line interface and a browser frontend for agent sessions,
with tool execution, configurable models, skills, and subagent delegation.

Local session creation and resume, `serve`, and `web` use the production service
and the security and capability bundle. The web frontend remains experimental.
Do not substitute an unrestricted or test/denying backend. Templates use
MiniJinja. Existing Scriban templates require manual conversion.

**Back up before opening existing state.** Incompatible databases are refused,
not migrated or reset. Use `parrot chat --new-session` to start fresh without
replacing the old session. See [state and rollback](docs/operator/state-and-rollback.md).
Do not run older versions against the same workspace concurrently. Older
versions may not observe current ownership records, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Documentation

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, backups, and rollback](docs/operator/state-and-rollback.md)
- [Use Gemini with an API key](docs/operator/gemini.md)
- [CLI Markdown rendering](docs/operator/markdown.md)
- [Benchmarking agent runs](docs/operator/benchmarking.md)
- [Design philosophy](docs/design-philosophy.md)
- [Domain glossary](GLOSSARY.md)

## Benchmark results

| 113-task cohort with error-only reruns | Reasoning effort | Passed | Failed tests | Errors | Pass rate | Cumulative input tokens including cached | Cumulative cached input tokens | Cumulative output tokens | Estimated cumulative cost |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| DeepSeek V4.1 Flash 1017 through DeepInfra FP8 | High | 69 | 43 | 1 | 61.1% | 1.114B | 1.083B (97.25%) | 12.98M | $31.27 peak; $15.63 off-peak |
| GLM-5.3-Flash through CoreWeave NVFP4 | High | 71 | 42 | 0 | 62.8% | 1.432B | 1.392B (97.21%) | 8.95M | $52.22 |
| MiMo V2.6 Flash through Cohere FP8 | High requested, semantics unverified | 61 | 52 | 0 | 54.0% | 2.152B | 1.981B (92.06%) | 17.93M | $34.49 |

Ten execution-error attempts were rerun on master `f71ad31c` on November 16, 2026.
The table replaces only their outcomes, without selecting the best attempt.
GLM Koota query predicates and MiMo Boa passed. Seven retries failed graded tests.
DeepSeek Pwntools again reached a verifier timeout. All six GQL and Participle
retries reached grading without the earlier attachment startup errors.

These are combined results across client revisions, not fresh full-suite runs.
Token counts and costs include original attempts and error-only reruns, including
MiMo's archived credit-blocked work. Prior raw results remain unchanged.
See the [error-only rerun report](/mnt/workspace/parrot-benchmark-reports/deepswe-errors-latest-master-2026-11-16/README.md).

Costs reprice captured provider token counts at official model-vendor API rates,
verified November 16, 2026. Sources are [DeepSeek pricing](https://api-docs.deepseek.com/quick_start/pricing),
[Z.AI pricing](https://docs.z.ai/guides/overview/pricing), and
[Xiaomi MiMo overseas real-time pricing](https://mimo.mi.com/docs/en-US/price/pay-as-you-go).
No OpenRouter serving-provider prices or discounts are used.

Input includes cache reads and repeated history transmissions. Root and child
agents are included. Cost equals uncached input times its rate, plus cached
input times its rate, plus output times its rate. These counterfactual estimates
preserve the observed token counts and cache hits, which can differ on vendor APIs.
They exclude separately billed vendor services and are not invoices.

DeepSeek charges half price off-peak. Peak hours are 01:00 to 04:00 and 06:00 to
10:00 UTC on weekdays, excluding Chinese public holidays. The table shows both
whole-run rate scenarios, not a timestamp-weighted bill. DeepSeek's provider
capture has more generations than its client ledger.

MiMo resumed after credit exhaustion. Its score preserves the 60 earlier completed
outcomes and uses continuation results for 53 eligible tasks, without best-of selection.
Before the latest-master error reruns, MiMo's cumulative official-rate estimate
was $33.61, including archived credit-blocked work. The table adds the four error
reruns to that cumulative usage, rather than dropping earlier failed-attempt costs.
Two tiny preflights are excluded. The endpoint accepted a high-effort request and
returned reasoning, but does not advertise effort-level control.

See the [MiMo continuation report](/mnt/workspace/parrot-benchmark-reports/deepswe-cohere-mimo-resume-2026-11-16/README.md)
and [CoreWeave GLM report](/mnt/workspace/parrot-benchmark-reports/deepswe-coreweave-glm-full-2026-11-15/README.md)
for outcome evidence and usage reconciliation. These runs do not isolate model
performance from serving-provider differences.

### DeepSeek through DeepInfra FP8

The November 15, 2026 DeepSWE run used DeepSeek V4.1 Flash 1017 through
OpenRouter's DeepInfra FP8 endpoint. Reasoning effort was `high`, concurrency
was 16, and benchmark retries were disabled.

The original full-suite pass rate was **61.1%**, with three execution errors.
After error-only reruns, two became graded failures and one verifier timeout remains.

Including the error-only reruns, the official-rate estimate is **$31.27 at peak
rates** or **$15.63 at off-peak rates**. These are vendor API pricing scenarios,
not the amount charged by DeepInfra.

See the [full run report](/mnt/workspace/parrot-benchmark-reports/deepswe-deepinfra-deepseek-full-2026-11-15/README.md)
for task results, usage reconciliation, immutable client provenance, and
verification commands.

## Design philosophy

See [Design philosophy](docs/design-philosophy.md) for the original principles
on bounded agent scope, delegation, parent attention, and model-tier selection.
Repository contributor guidance is in [AGENTS.md](AGENTS.md).

## Core and runtime

Agent turns process admitted user input until the assistant completes, a request
budget is exhausted, or an error stops the turn. Idle sessions do not initialize
a provider request or record a turn start. Model and profile settings stay fixed
for each active turn, and its executable tools are captured once.

### Input and completion

Steers can join an active turn. Queued messages wait until admission permits a
new turn. Inspecting pending input does not reserve or consume it, and steers
admitted together remain in one batch.

A no-tool assistant response is a completion candidate, not proof that the turn
has closed. Outstanding work and reminders can block completion, even when a
blocker's message text is empty. The candidate is retained once, blocker messages
are recorded in order, and the assistant continues with a renewed request budget.
Accepted mutations finish before completion checks and final closure.

Provider-request limits trigger the final request phase. Request or tool-settlement
limit exhaustion produces a recorded failure rather than a successful completion.
Scheduled session operations run between turns. Unscheduled turns share their
request budget until a scheduled-work boundary ends that budget scope.

### Context, skills, and compaction

Requests include the selected instructions, conversation, tool definitions, and
pending context that fits the model's limits. Token estimates include native
text tokenization and provider-specific image costs. Usage calibration survives
later streaming or publication errors and resets only after successful compaction
persistence.

Skill mentions are resolved for each admitted input. Unknown references permit
one catalog refresh per lookup batch. Skill identities are deduplicated per turn
before instruction bodies are read, then selected-skill ordering and prompt-size
limits apply. Templates use their configured engine and declared arguments.

Automatic, explicit, and oversized-request compaction follow the same retention
rules. They protect incomplete tool-call groups, respect checkpoints, and summarize
a safe history prefix. Large tool groups can be stored as blobs and replaced with
notices whose actual text must fit. Empty summaries trigger recursive fallback.
Final status fitting permits at most eight rerenders after the initial rendering.

Compaction reads the previous summary, its post-watermark durable tail, active
checkpoints, and current messages separately during a serialized epoch. Core
uses the summary watermark to bound the tail and assembles the state without
a combined atomic snapshot or cached capture. The tail retains tool ownership
and status metadata. Current transient notices remain distinct from durable
history. A successful compaction persists its summary and watermark before
reloading the visible conversation. Summarized history remains in the database.
A preparation failure after compaction starts reports failure without recording
a replacement. Oversized requests allow only one compaction retry.

### Tools, recovery, and accounting

Truncated tool calls and invalid JSON arguments are rejected before recording or
execution. Valid assistant calls are recorded before tools run. Only calls from
a new model response enter execution. Tool batches preserve parallel-safe grouping
and tool-specific budgets, with each group's results recorded before the next
group starts.

Cancellation reaches executing tools, and admitted tool work is joined before
execution returns. Advertised tool definitions do not substitute for executable
tools or grant permissions.

On recovery, a historical tool call without a saved result receives a durable
error stating that the process crashed before the tool completed. It is not
automatically executed again. Existing results are restored without modification
or republication. A crash error does not mean the tool made no external changes.

Session operations are serialized. Usage from an observed streamed completion is
recorded durably even if the stream later fails. Provider-specific extensions and
multimodal content are preserved across request and history conversion.

## Compiler cache

`nix develop` provides `sccache` and sets `RUSTC_WRAPPER` to use it. The compiler
cache uses sccache's per-user default cache directory, shared across worktrees.
Cargo build artifacts remain worktree-local in `.tmp/cargo-target` unless
`CARGO_TARGET_DIR` is set. Cargo's dependency cache also uses its normal per-user
location unless `CARGO_HOME` is set externally.

Rust incremental compilations are not cacheable by sccache. To favor compiler
cache reuse across clean targets or worktrees, run `CARGO_INCREMENTAL=0 cargo build`
(or set that variable for the desired session). This does not make every Rust
compilation cacheable. For example, linking binaries is not cached. Use
`sccache --show-stats` to inspect cache hits and non-cacheable compilations.

Development and test builds keep file and line information for backtraces, but
omit debugger local-variable and type information. Set
`CARGO_PROFILE_DEV_DEBUG=2` for `cargo build` or `cargo test` when you need full
debug information. The release profile is unchanged.

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
compaction-artifact fixtures use `std::env::temp_dir()`. Ensure that `TMPDIR` is
on a local filesystem supporting atomic rename exchange, not an NFS mount. Keep
that path short. Native host fixtures append session and listener names that must
fit within the platform's Unix socket path limit. `nix develop` points `TMPDIR` at
`/tmp/nix-shell.XXXXXXXX`, which is long enough to overflow that limit, so run the
native suites with `TMPDIR=/tmp`. Production artifact quarantine
also requires atomic rename exchange. This test setup does not establish NFS
support.
Additional native-platform and package-smoke gates are listed in
[.github/workflows/check.yml](.github/workflows/check.yml).

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Benchmarking with headless runs](docs/operator/benchmarking.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

### ChatGPT fast mode

Use `/fast` to toggle priority inference for ChatGPT subscription requests, or
`/fast on` and `/fast off` to select it explicitly. `/status` reports the current
selection. The choice is saved as the default for new sessions in your YAML
configuration:

```yaml
fast_mode: true
```

Fast mode defaults to `false`. It keeps the selected model and reasoning effort
unchanged, but consumes more of your ChatGPT subscription allowance. Availability
and acceleration depend on the selected model and your account. Other providers
are unaffected.

The selection belongs to the user session and is shared by its root agent and
subagents. Changes apply to subsequent requests, including requests from already
running subagents. They do not change a request already in flight. Resuming a
session restores its own saved selection rather than replacing it with the
current default.

### Steering a running agent

In interactive chat, messages submitted while the agent is busy stay in the live
buffer as `◇ … (steer pending)`. They move to scrollback as `◆ …` only when the agent
takes them into its conversation at the next turn boundary. A new response or a
turn ending alone does not mark a pending steer as consumed.

### HTTPS certificates for `serve`

HTTPS serving uses a Rust-native TLS stack from Tonic and Rustls, with
explicitly supplied PEM files:

```sh
parrot serve --listen https://127.0.0.1:7443 \
  --token-file /run/parrot/token \
  --tls-cert-file server-chain.pem \
  --tls-key-file server-key.pem
```

Both TLS flags are required for HTTPS and rejected for HTTP or Unix-domain-socket
serving. The certificate file contains the PEM certificate chain, with the leaf
first. The key file contains its matching **unencrypted PEM private key**. Relative
paths resolve against the process working directory. Invalid certificates or keys
fail startup before the listening announcement. Keep the private key readable only
by the server owner.

There is no certificate discovery, generation, certificate-store lookup, PFX,
PKCS12, or encrypted-key support. TLS provisioning does not read environment
variables or `appsettings*.json`. Files are loaded at startup, not through hot
reload. HTTPS still requires the owner-only bearer token file. Client certificate
and hostname verification are never disabled.

## Acknowledgments

Parrot's behavioral guidelines and bundled skills include work from:

- **Lauren Tan (poteto)**, author of [pstack](https://github.com/cursor/plugins/tree/6b0300214a09fd6029cfb7077043125758cd1fd2/pstack).
  The [behavioral guidelines](assets/config/system-prompts/behavior-guidelines.md)
  and [architecture skill](assets/skills/architecture/SKILL.md)
  adapt pstack's principles, workflow techniques, and writing guidance
  from that pinned revision to Parrot's tools and configured models. The bundled
  `how`, `interrogate`, `prove-it-works`, `reflect`, `technical-writing`,
  `test-behavior-not-implementation`, `why`, and `unslop` skills also come from
  pstack. These adaptations do not install or provide pstack's scripts or
  Cursor-specific integrations.
- **Matt Pocock**, author of [mattpocock/skills](https://github.com/mattpocock/skills).
  Parrot copies or adapts `domain-modeling`, `grill-with-docs`, `grilling`, and
  `wayfinder` from that collection.
- **OpenAI**, for the [openai/skills](https://github.com/openai/skills) collection.
  Parrot bundles adapted versions of `imagegen`, `openai-docs`, `skill-creator`,
  and `skill-installer`.

License notices are retained in [third-party notices](THIRD_PARTY_NOTICES.md)
and the individual skill directories under [`assets/skills/`](assets/skills/).
