# 0004: Task leaf responses validate at the completion blocker

Date: 2026-11-23
Status: Accepted

## Context

A dedicated task worker must end its turn with a leaf response, a strict JSON
object with `result` and a `verdict` (`accept`, `reject-and-retry`, or
`reject-and-halt`). The parser is `TaskArtifactParser::parse_leaf_response` in
`crates/parrot-tools/src/task_artifacts`.

The task runner used to validate after the turn closed. `TaskRunner::run_and_parse`
parsed the completed output. On a parse failure under the budget it replaced the
prompt with the `agent-task.response-repair` text, allocated a new prompt message
id, recorded the `RepairingResponse` phase, cleared the response reference, and
ran another turn. The worker session already runs the same `TurnEngine` as every
other session, and the turn loop already has an exit interception point:
`collect_completion_blockers()` in `crates/parrot-core/src/turn.rs`.

## Decision

The turn engine's completion blocker inspects the candidate final response. A
leaf response blocker validates the candidate with the existing parser. An
unparseable candidate blocks the exit, injects the rendered parse error, and
forces the model to continue in the same turn. The runner loses its repair loop;
it parses once and keeps only the attempt loop for `reject-and-retry` verdicts.

The blocker is active for sessions whose selection profile is the task payload
profile (`agent-task-payload`, the constant the scheduler sends when it spawns
workers). Profile-based behavior follows the existing precedent in the scope
factory, which resolves modes by profile id.

## Consequences

- A completed worker turn carries a parseable leaf response, unless the repair
  budget is exhausted. On exhaustion the blocker stops blocking, the turn closes,
  and the runner's parse gate fails the task with the existing
  "leaf response invalid" summary.
- The persisted runner checkpoint lost `repair_attempt` and
  `maximum_response_repairs`, and the `RepairingResponse` phase is gone.
  Persisted agent state moved from schema version 2 to 3, with a normalize step
  in storage that maps a persisted `repairing_response` phase to
  `prompt_intent`.
- The repair budget counter is session-scoped and in memory, reset when a turn
  opens. A restart mid-turn grants a fresh budget. Identical repeats still trip
  stall prompts, and the request budget bounds the turn.
- The blocker validates every finish attempt of a task payload session. A
  steered mid-turn response gets the same treatment as before, where the runner
  re-prompted a parse error for it.

## Alternatives considered

- Keep the runner repair loop. Rejected because it duplicates the exit
  interception mechanism the turn loop already has, and it keeps a phase, prompt
  replacement, and per-repair durable bookkeeping in the runner.
- Thread the candidate through shared state instead of the protocol. Rejected
  because hidden mutable state between the loop and the blockers is harder to
  reason about than an explicit parameter.
- Drop the retry counter and rely on stall detection alone. Rejected because
  varying unparseable outputs would burn the whole request budget instead of
  three retries, and the failure would surface as budget exhaustion instead of
  the crisp leaf response failure.
