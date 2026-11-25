# 0005: Caller-supplied output schemas gate the root session at the completion blocker

Date: 2026-11-25
Status: Accepted

## Context

A one-shot `parrot chat` command should end with a machine-readable result. The
caller supplies a JSON schema, inline or from a file, and the command must not
complete while the agent's final response fails that schema.

The turn engine already has an exit interception point:
`collect_completion_blockers()` in `crates/parrot-core/src/turn.rs`, fed by
`CompletionStateSource` implementations constructed per agent session in the
scope factory. ADR 0004 used the same mechanism for the task-worker leaf
response blocker, which validates the candidate final response, blocks the
exit, injects a rendered error, and lets the model continue in the same turn up
to a repair budget.

The CLI creates or resumes the user session for a one-shot run, so the schema
must reach the session through the create or resume request. The caller also
owns telling the agent what shape to produce; the feature injects no upfront
instruction.

## Decision

A caller-supplied JSON schema attaches to the user session at create or resume
through an `output_schema` field on `CreateSessionRequest` and
`ResumeSessionRequest`. The schema text is in-memory runtime state, not
persisted session data. An empty string means no schema, so a later resume
without the flag clears the gate.

The host compiles the schema when the session opens. Invalid schema text fails
create and resume with an invalid argument error, so a bad schema surfaces
before the first prompt.

A new output schema blocker validates the candidate final response, the turn
result, against the compiled schema. A failing candidate blocks the exit,
injects the rendered `output-schema.repair` message with the validation error
and the schema text, and lets the model continue in the same turn, up to the
new `runtime.output_schema.maximum_response_repairs` budget. After the budget
the blocker stops blocking, the turn closes, and the one-shot command exits
non-zero because the CLI re-validates the final response for the exit code.

The blocker is constructed for the root agent session only. A child agent
session's output is not the command's result. The caller explains the schema in
its own prompt, so the first attempt already targets it without an injected
instruction.

## Consequences

- A one-shot run against a schema-attached session closes only with a
  conforming final response, or closes invalid once the budget is spent and the
  command fails.
- The gate applies to every finish attempt of the root session, including
  steered mid-turn responses, the same as the leaf response blocker.
- The schema is runtime state. A process restart loses it until the client
  resumes with the flag again.
- The budget key is separate from `agent_tasks.maximum_response_repairs`
  because this gate is not agent-task specific. The default of 3 matches the
  existing agent-task behavior.
- The blocker is inert without a schema, so existing sessions, subagents and
  the leaf response blocker keep their behavior.

## Alternatives considered

- Attach the schema per prompt on `SendMessageRequest`. Rejected because the
  gate would need per-turn dynamic state, while a session-attached schema flows
  through the existing create and resume requests and covers both a fresh and a
  resumed one-shot session.
- Inject a templated upfront instruction with the schema. Rejected because the
  caller owns explaining the expected output shape, and an injected instruction
  would fight the caller's own prompt.
- Validate only in the CLI after the turn closes. Rejected because the agent
  would never see the validation error and could not correct its output, and
  the runner-side post-hoc loop is exactly the duplication ADR 0004 removed.
- Reuse `agent_tasks.maximum_response_repairs`. Rejected because the budget
  belongs to the output schema feature, not to agent tasks.
