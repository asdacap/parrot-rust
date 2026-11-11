# pier-parrot-agent

A [Pier](https://github.com/datacurve-ai/pier) agent wrapper that benchmarks the
**Parrot** coding agent (the Rust port in this repository) on Harbor-format task
suites such as [DeepSWE](https://deepswe.datacurve.ai/run).

## How it works

Parrot ships a one-shot chat path: `parrot chat --basic --new-session "<task>"`
runs exactly one agent turn and exits 0 only when the turn completes. The
wrapper installs a benchmark build of the `parrot` binary in the sandbox, runs
that path with Pier's rendered instruction, and converts the machine-readable
event log into Pier's augmented ATIF v1.7 trajectory.

```sh
uv tool install git+https://github.com/datacurve-ai/pier
uv tool install -e ./pier            # or: pip install -e ./pier

git clone https://github.com/datacurve-ai/deep-swe
export OPENAI_API_KEY=...

pier run -p deep-swe/tasks --agent-import-path parrot_agent:ParrotAgent \
    --model openai/gpt-5.5 --env docker
```

Useful agent kwargs (`--ak key=value`):

- `binary_url` — tarball to install. Default points at this repository's
  GitHub release `parrot-x86_64-unknown-linux-musl.tar.gz`. The tarball must
  contain the `parrot` binary at its root. The download host is added to the
  network allowlist, so a local HTTP server works under filtered egress.
- `binary_sha256` — expected sha256 of the downloaded tarball.
- `provider_config` — JSON object written to
  `~/.config/parrotrust/config.yaml` under `providers:` for custom gateways,
  e.g. `--ak 'provider_config={"respan":{"type":"openai-compatible","protocol":"responses","base_url":"https://gw.example.com/api","api_key_env":"GW_KEY"}}'`.
  An existing `config.yaml` is never overwritten; pre-bake one instead.
- `mode` — optional Parrot foreground profile passed as `--mode`.

Provider API keys are passed from the Pier job environment to the sandbox
process (`agent.env`, or the host environment) matched by the model's provider
id, e.g. `openai/...` needs `OPENAI_API_KEY`; `provider_config` entries can
name their own `api_key_env`. OAuth-backed providers such as `chatgpt` have no
environment key, so use API-key providers for benchmark runs.

The network allowlist contains the provider endpoints (from the provider id
and `provider_config` base URLs), `models.dev` (Parrot fetches the model
catalog from it at startup, needed for unknown models), and the `binary_url`
host. The `--model` value must be a fully qualified `provider/model`; the
config's `model_aliases` are not reachable in a fresh sandbox.

## Files produced per trial

Inside the sandbox at `/logs/agent/` (Pier downloads them into the trial's
`agent/` directory):

- `parrot.txt` — merged stdout/stderr of the one-shot run (human-readable).
- `parrot.jsonl` — the event log consumed for trajectory conversion.
- `trajectory.json` — generated after the run from `parrot.jsonl`.

## The `parrot.jsonl` contract

Emitted by `parrot chat --jsonl <path>`. One JSON object per line, in stream
order, LF-separated, append-only, flushed per line:

```json
{"id": "<event id>", "agent_session_id": "<agent session>", "payload": {"type": "<payload kind>", ...}}
```

- `type` is the exact snake_case oneof field name from `proto/parrot.proto`
  (`turn_started`, `text_chunk`, `reasoning_chunk`, `tool_call_chunk`,
  `tool_started`, `tool_finished`, `tool_error`, `tool_cancelled`,
  `request_usage_recorded`, `turn_ended`, `turn_failed`,
  `compaction_started`, ...).
- Remaining payload fields are that proto message's snake_case fields.
  Optional proto fields are omitted when unset. Proto enums serialize as
  integers (`reasoning_chunk.kind`: 0 raw, 1 summary).
- Repeated message fields, nested message fields (e.g. `yielded_process`,
  `artifacts`) and bytes fields are omitted. Repeated scalar fields (e.g.
  `tool_call_ids`) are kept as string arrays, so tool results are read from
  `tool_finished.result` only.
- Unknown payload types and keys may appear in the future; parsers must be
  tolerant.

## Trajectory conversion rules

- One **user** step per `input_admitted` event (the rendered instruction).
- One **agent** step per provider request, bounded by
  `request_usage_recorded`. Streamed `text_chunk` fragments concatenate into
  the step message; `reasoning_chunk` fragments into `reasoning_content`;
  assembled `tool_call_chunk` fragments into structured tool calls.
- Tool results (`tool_finished.result`) are attributed to the request whose
  `tool_call_ids` own that call, so the step that issued a tool call also
  carries its observation.
- Step metrics come from the request's usage record (`prompt_tokens`,
  `completion_tokens`, `cached_tokens`, `cost_usd = input_cost +
  output_cost`, and `context_tokens`/`context_limit` in `extra`).
- `final_metrics` sums all usage records; `peak_context_tokens` is the largest
  observed `context_size` (falling back to the largest step prompt token
  count); `summarization_count` counts `compaction_started` events.
- `turn_failed` / `agent_failed` messages do not become steps; the CLI exits
  nonzero on failure and Pier records the exception.

## Known limitations

- Subagent (background agent) trajectories are filtered out of the main
  trajectory; only the main agent session's events are converted.
- Events carry no wall-clock timestamps, so ATIF steps have none.
- Events after the last usage record form a final step without usage metrics.
