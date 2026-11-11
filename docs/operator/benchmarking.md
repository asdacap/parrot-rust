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

Leaderboard submission is handled by the benchmark maintainers; see the
DeepSWE run page for contact details.
