# Benchmarking with headless runs

Run Parrot as a one-shot agent for coding-agent benchmarks such as
[DeepSWE](https://deepswe.datacurve.ai/run), which drives a CLI agent inside a
sandboxed task environment through [Pier](https://github.com/datacurve-ai/pier).
A benchmark run gives Parrot one task prompt, lets it work to completion, and
records what it did.

## Webless build

Benchmarks need a self-contained binary. Build one without the embedded web UI
so the build needs no retained frontend output:

```sh
cargo build -p parrot-cli --no-default-features --locked
```

This produces a chat-and-serve-only `parrot` binary. The `web` cargo feature
embeds `web/dist` and is on by default; a build without it refuses `parrot web`
with a clear error and otherwise behaves the same.

For a fully static release binary suitable for sandbox installs, build the same
feature set for a musl target (the target must be installed, and cross
compilation needs a matching musl C compiler for the bundled PTY helper):

```sh
cargo build -p parrot-cli --no-default-features --locked \
    --target x86_64-unknown-linux-musl --release
```

Package the resulting `parrot` binary at the root of a gzip tarball named
`parrot-x86_64-unknown-linux-musl.tar.gz` for the Pier install step below.

## One-shot chat

`parrot chat --basic --new-session` with a task prompt or piped stdin runs
exactly one agent turn and exits. The exit code is 0 when the turn completed
and 1 when it failed:

```sh
parrot chat --basic --new-session --model openai/gpt-5.5 "Fix the failing test"
```

Add `--jsonl <file>` to write a machine-readable event log of everything the
session emitted. The file is append-only, one JSON object per line, flushed as
each event arrives. Only the one-shot path writes the log.

Each line has this shape:

```json
{"id": "...", "agent_session_id": "...", "payload": {"type": "tool_started", "tool_call_id": "...", "tool_name": "..."}}
```

The `payload.type` key is the snake_case oneof field name from
`proto/parrot.proto` (`Event.payload`), for example `turn_started`,
`text_chunk`, `reasoning_chunk`, `tool_call_chunk`, `tool_started`,
`tool_finished`, `turn_ended`, `turn_failed`, `request_usage_recorded`, and
`session_usage_snapshot`. All remaining field names are the proto field names.
Payload values follow these rules:

- Optional fields are omitted when unset.
- Proto enums serialize as their integer value.
- Repeated and nested message fields (for example `yielded_process` and
  `artifacts`) are omitted. Repeated scalar fields such as `tool_call_ids`
  remain arrays.
- Events appear in stream order. `RequestUsageRecorded` carries per-provider-
  request token counts, costs, and tool call ids, which trajectory consumers
  use for context and cost metrics.

## Running DeepSWE with the Pier wrapper

The `pier/` directory in this repository holds a small Python package with a
Pier agent for Parrot. Install Pier, clone the benchmark tasks, and point Pier
at the wrapper:

```sh
uv tool install git+https://github.com/datacurve-ai/pier
git clone https://github.com/datacurve-ai/deep-swe
pip install -e ./pier

export OPENAI_API_KEY=...
pier run -p deep-swe/tasks --agent-import-path parrot_agent:ParrotAgent \
    --model openai/gpt-5.5 --env docker
```

The wrapper installs Parrot from a release tarball URL (default:
`https://github.com/asdacap/parrot-rust/releases/latest/download/parrot-x86_64-unknown-linux-musl.tar.gz`,
override with the `binary_url` and `binary_sha256` agent kwargs), runs
`parrot chat --basic --new-session --jsonl /logs/agent/parrot.jsonl` inside the
task directory, and converts the JSONL event log into Pier's trajectory format
for scoring and the trajectory viewer. Provider API keys are passed through the
agent `env` block, the network allowlist is derived from the model's provider
endpoint, and a `provider_config` agent kwarg (the same provider shape as
Parrot's YAML config) can point the model at a gateway or custom endpoint.

Start with a single task while wiring everything up:

```sh
pier run -p deep-swe/tasks/<task-id> --agent-import-path parrot_agent:ParrotAgent \
    --model openai/gpt-5.5 --env docker
```

### Credentials and model selection

Provider API keys flow from the Pier job environment into the sandbox through
the agent `env` block or `--env-file`, matched by the model's provider id
(`openai/...` takes `OPENAI_API_KEY`). A `provider_config` agent kwarg can add
a gateway with its own `api_key_env`. OAuth-backed providers such as `chatgpt`
have no environment key, so prefer API-key providers for benchmark runs.

Pass the model as a fully qualified `provider/model` string. Parrot resolves
it against the configured providers plus the models.dev catalog, which it
fetches eagerly at startup; the wrapper allowlists `models.dev` for that. The
config's `model_aliases` are user-session state and are not reachable in a
fresh sandbox.

### Iterating with a local build

Pier bakes the agent install into a per-trial docker image keyed by the
install spec fingerprint, so repeat runs with an unchanged install spec reuse
the image. To benchmark an uncommitted local build, serve the tarball from the
host and point `binary_url` at it:

```sh
cargo build -p parrot-cli --no-default-features --locked
tar -czf /tmp/parrot.tar.gz -C target/debug parrot
python3 -m http.server 8765 --bind 0.0.0.0 --directory /tmp &

pier run -p deep-swe/tasks/<task-id> --agent-import-path parrot_agent:ParrotAgent \
    --model openai/gpt-5.5 --env docker \
    --ak binary_url=http://<host-ip-on-docker-bridge>:8765/parrot.tar.gz
```

Use the host IP on the docker bridge (often `172.17.0.1`) or an address the
container can reach; the wrapper adds the `binary_url` host to the network
allowlist. A local URL only works with `--env docker`; for `--env modal` push
the tarball to a URL the sandbox can reach.

### Running Terminal-Bench

[Terminal-Bench](https://www.tbench.ai/run) currently recommends version 4.1.0.
The separate job configuration in `pier/terminal-bench.yaml` uses the existing
Parrot wrapper, Modal sandboxes, and five attempts per task. It does not change
DeepSWE or FrontierSWE runs. Some tasks require GPUs, so use a GPU-capable
sandbox account for the full suite.

Pier reads local tasks but does not download Harbor registry datasets. Install
both runners, then download the pinned dataset with Harbor. Run these commands
from the repository root:

```sh
uv tool install 'harbor==0.25.0'
uv tool install 'datacurve-pier==0.3.1' --with-editable ./pier
harbor download 'terminal-bench/terminal-bench@4.1.0' \
    -o pier/datasets/terminal-bench-4.1.0
```

Authenticate Modal, then export the API key for your selected model and start
the job:

```sh
uv tool run --from 'datacurve-pier==0.3.1' modal setup
export OPENCODE_GO_API_KEY=...
pier run --config pier/terminal-bench.yaml
```

The configuration uses `opencode-go/glm-5.3-flash/high`, matching the previous
DeepSWE run. To use another model, edit `agents[].model_name` and its
`kwargs.provider_config` in the YAML, then export the matching API key. Use a
fully qualified `provider/model`. Pier does not apply `--model` alone to agents loaded
from a configuration file. The wrapper uses the latest published Parrot musl
binary unless you pass `--ak binary_url=<url>`.

For an initial single-task, single-attempt run:

```sh
pier run --config pier/terminal-bench.yaml \
    --path pier/datasets/terminal-bench-4.1.0/terminal-bench \
    --n-tasks 1 --n-attempts 1
```

The configuration allows four concurrent trials and preserves the tasks'
resource requirements. Dataset downloads under `pier/datasets/` are ignored by
Git. Results use Pier's default `jobs/` directory and can be inspected with
`pier view`. This runs Terminal-Bench through Pier, not the official Harbor
agent runner. Check the benchmark's submission requirements before comparing
or submitting results.

### Running FrontierSWE v2

[FrontierSWE v2](https://github.com/Proximal-Labs/frontier-swe-v2) ships
Harbor-format tasks, so the same Pier command shape applies. Pier currently
rejects the `network_mode = "allowlist"` value these tasks declare at parse
time, so clone the tasks and patch the agent-phase policy to `public` first:

```sh
git clone https://github.com/Proximal-Labs/frontier-swe-v2
sed -i 's/network_mode = "allowlist"/network_mode = "public"/' \
    frontier-swe-v2/tasks/*/task.toml

pier run -p frontier-swe-v2/tasks --agent-import-path parrot_agent:ParrotAgent \
    --model <provider>/<model> --env docker
```

With `public`, the agent has unrestricted internet and the wrapper's
allowlist is inert. The tasks' `no-network` declarations are verifier-side
only; Parrot never runs there. Expect heavy resource requirements per task
(up to 16 CPUs, 131 GB RAM, 512 GB storage, and GPUs on some tasks), and a
72000-second agent timeout from each `task.toml`. Parrot's per-turn provider
request cap comes from the selected profile's `max_turns` (1024 for the main
profiles in `predefined_config.profiles.yaml`). Running the benchmark's
intended harness, `px-eval`, would need the wrapper repackaged as a Harbor
agent directory instead of a Pier import path.

Leaderboard submission is handled by the benchmark maintainers; see the
DeepSWE run page for contact details.
