# 0001: Turn identity is the session's latest open turn

Date: 2026-11-15
Status: Accepted

## Context

Closing a turn is a durable arbitration: the committed event stream must hold
exactly one terminal per turn, and normal-completion, interrupted, and
retry-after-crash writers can all propose a terminal for the same turn. The
arbitration keyed each proposal by a `TurnKey` of `(agent_session_id,
start_event_id)`, where `start_event_id` is the persisted id of the turn's
`TurnStarted` event. Storage looked the start row up by id and scanned forward,
decoding every payload to find a terminal or a newer turn start. The event
table stores payloads as opaque blobs, so no type-filtered query was possible,
and the publisher had to return the generated event id to the turn loop so the
loop could carry it into the close path.

## Decision

1. The `event` table gains a `kind` column holding the event payload's oneof
   variant name, derived at insert time the same way `request_id` and
   `tool_call_id` are extracted. A partial index covers turn lifecycle kinds.
2. Close-turn arbitration drops the start event id. A proposal carries only the
   agent session id. Arbitration finds the session's latest `turn_started`
   event, reports `AlreadyClosed` if a `turn_ended` follows it, appends the
   terminal otherwise, and fails when no turn start exists.
3. `TurnKey` disappears; the close-turn storage APIs take the agent session id.
4. The start event id is no longer returned by
   `EventPublisher::report_turn_started`, no longer carried in
   `TurnServices`/`ExecutionServices`, and no longer part of the
   `CloseTurn` persistence request. Turn diagnostics lose the per-turn
   `correlation_id`.
5. The schema version bumps to 11. Databases at version 10 are rejected at open,
   matching the repo's precedent of replacing the session format instead of
   migrating in place.

## Consequences

- A retried terminal from an older crashed turn now closes the session's latest
  open turn instead of failing as stale. The runtime schedules one turn per
  agent session at a time, so in practice only a crash window exposes this.
- Multiple unclosed `TurnStarted` events can exist per session; "the open turn"
  means the latest one.
- Turn start/finish diagnostic events no longer pair by correlation id.
- Existing user databases stop opening; session history is not migrated.
