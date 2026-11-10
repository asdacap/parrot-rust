# Retained task scheduling

Each agent owns its task graph. Task state, results, failures, visibility, worker
identities, retry progress, and undelivered notifications survive session restart.
Reopening does not reset terminal tasks or reread mutable task artifacts as new
instructions.

## Assignment and accepted responses

Each owner receives at most one ready ordinary instruction leaf as self work, in
stable graph order. Composites and tasks with model overrides always use dedicated
workers. Other ready parallel work uses dedicated workers as capacity permits.
Existing dedicated assignments do not change when later self work becomes ready.
Later self assignments arrive automatically without another setter call.

Self work must record its outcome and result through `set_agent_tasks`. A final
chat response or JSON object does not complete it. Dedicated workers complete
through strict final JSON. A composite worker registers and resolves its own
inner graph before completing its parent task. The parent's snapshot does not
contain the child's graph.

List and detail queries expose read-only `assigned_to`, either `self` or an
absolute registered dedicated-agent path. Never-assigned tasks omit it. Terminal
tasks retain their last assignee until reset. `agent_name` remains a compatibility
alias for dedicated workers. Neither field is accepted task input.

New accepted updates save their rendered response before returning it. Recovery
replays those exact bytes before rereading arguments or mutable artifacts, even
if the graph has since advanced. Queries and later notifications describe current
state. Older count-only receipts retain their original count response.

## Updates, cancellation, and visibility

Accepted graph updates take effect in admission order. Dropping a caller's
observer does not cancel an already admitted update, publication, or worker run.
Reads wait for earlier updates and do not expose a partially constructed worker.
They can observe canceled graph state while cleanup is still finishing.

Failed tasks hold their dependents. Cancellation affects transitive dependents.
Resetting a succeeded task invalidates its transitive running, succeeded, and
failed dependents. An old canceled or restarted run cannot overwrite its
replacement's outcome. Completion preserves current visibility rather than
restoring the visibility captured when execution began.

When a nonempty graph becomes entirely succeeded or canceled, its tasks become
hidden. Explicitly unhiding a resolved graph remains effective until a later
transition into the fully resolved state. Hidden self tasks remain hidden.
Temporary visibility for dedicated activity identifies its worker, not the owner.

Explicit settlement rejects later graph updates, cancels unresolved work, joins
admitted executions and stop records, and drains notifications. Repeated
settlement observes the same cleanup. Cleanup continues through failures rather
than abandoning later work. The first error follows shutdown publication,
previous execution failures, remaining executions in start order, then notifier
cleanup. Later failures remain available as bounded diagnostic codes.

Diagnostics do not contain prompts, exception bodies, task or model names, or
policy data. Successful cleanup does not emit failure diagnostics.

## Recovery and shutdown

Recovery reconnects saved workers to retained direct children. Saved selectors
resolve against current configuration; credentials and live security grants are
not restored. Each unfinished dedicated attempt continues with its existing
worker, prompt identity, retry and repair counters, feedback, and carried result.
It does not spend a new attempt merely because the session reopened.

A completed response saved before shutdown can be consumed without requesting it
again. Accepted outcomes are saved before dependents launch. A self assignment
remains owner work on recovery rather than becoming a child execution.

Normal hosted shutdown suspends unfinished work instead of explicitly canceling
the graph. It stops new updates, launches, and notification wakes while preserving
recoverable notifications and execution intent. Explicit interruption, task
cancellation, and superseding updates keep their cancellation semantics.
Succeeded, failed, and explicitly canceled tasks remain terminal on reopening.

Notifications are saved before delivery and reuse their message identity after
interruption. A snapshot write failure is reported and blocks later transitions
rather than starting unrecorded work. Snapshot files and SQLite are not one
transaction, and recovery does not guarantee exactly-once external effects.

## Snapshot compatibility

Recovery snapshots use version `2`; AgentTask input artifacts use version `1`.
Compatible version-`1` recovery snapshots convert once. Conversion stops and
joins obsolete attempts and their nested work before reassignment. Terminal tasks
and histories remain intact. Reassigned unfinished work can repeat partial
external effects.

Ambiguous legacy pending input blocks opening before provider execution,
cancellation, claim release, or input consumption. Its original bytes remain
unchanged. Resolve that exact input using compatible tooling or a compatible
version. Do not discard pending input, match prompt text to guess identity, or
bulk-clear inputs.

Back up the complete stopped session before upgrading. Older binaries may reject
version-`2` snapshots even when the database format has not changed. Do not edit
version fields or downgrade in place. Restore a complete pre-migration backup
with its matching binary and workspace. External effects are not rolled back.
See [operator recovery](../../../../docs/operator/state-and-rollback.md#agenttask-snapshot-migration).

## Dedicated-run concurrency

`agent_tasks.extra_concurrent_tasks` defaults to `2` and controls extra dedicated
runs shared across the user session. Each owner has one free concurrent dedicated
run, including owners of nested graphs. Self work consumes no child capacity, so
`0` permits one self task and one free dedicated run per owner. Internal retries
retain their admission. Direct `agent_spawn` calls are outside this limit.

Pending work starts automatically when capacity becomes available. Increasing the
limit wakes eligible work. Decreasing it does not stop ongoing runs; later extra
starts wait until usage falls below the limit. Each owner's free allowance remains
available. Saved running work resumes even if it exceeds a reduced limit.

`/task-limit` queries the current session limit without writing an override.
`/task-limit N` accepts one decimal integer from `0` to `2147483647`. The override
is persisted in the selected session's `meta.json` before the live limit changes
and survives reopening under a different global default. There is no reset
command or automatic reload of global configuration.

## Attempts and notifications

Dedicated-run defaults allow five verdict attempts and three response repairs per
attempt, with dependency-first fork history. Repairs do not consume verdict
attempts. A rejection can replace an instruction, but not a composite, carry a
replacement result, and append feedback. Self work has no automatic verdict or
repair loop.

Context and dependency summaries are bounded to 16 Ki UTF-16 units. The prompt
header budget is 256 Ki UTF-16 units minus its suffix length. The truncation
notice is outside that budget, and an already oversized suffix remains whole.

Notifications preserve enqueue order. Oversized text is stored as an output blob
before delivery. Each notification retains one message identity across three
attempts with one-second retry intervals. Once notification settlement starts,
remaining messages are recorded without waking the recipient.

These scheduling contracts do not establish production filesystem-security,
native-platform, or release acceptance. Deferred security checks and known
blocked filesystem operations remain outside the component's verification.
