# Completion blockers, reminders, and status

An agent cannot finish normally while its current completion checks report
unfinished work, pending asks, or exit reminders. Observation does not claim,
settle, cancel, retire, or clear the work it reports.

## Completion and waiting

Pending, running, and failed tasks block completion even when hidden. Completed
and canceled tasks do not. Child activity includes active direct children only.
Process blockers depend on the scoped process observer, not a global operating
system scan. File-based skill queues have no separate runtime blocker.

Checks preserve configured section and reminder order. A present result with empty
text still blocks completion. Any nonempty completion-feedback list retains the
assistant's candidate response and forces continuation, including an active-work-only
list. All completion categories are observed, so reminder render counts advance
even when an earlier category also blocks completion.

Accepted reminder mutations finish before the next completion observation and
final closure. The first feedback message records the candidate with its notice;
later feedback notices can be retained without being durable event records.
Retained publication must not be mistaken for proof of database commitment.

Waiting wakes on admitted input, child completion, task-graph completion, process
completion, or question notification. It does not subscribe to arbitrary inventory
changes or queue-file writes. Duration arguments reject unknown fields,
nonobjects, nonintegers, and values outside the supported range. Wait cancellation
does not erase blockers. User interruption remains distinct from successful normal
completion.

## Exit reminders

Reminder titles and descriptions are literal text. Empty titles fail when the
change is appended. Setting an identical description still records and publishes
a change, but does not reset its render count. A new or changed description and
clearing an existing title reset the count. Clearing an absent title does neither.

Replacing a live title keeps its position. Reopening replays each change by
removing the old title and appending its replacement, so order can differ after
recovery. Malformed recorded reminder data is an opening error rather than an
ignored entry. Rendering uses configured templates and ordinal numbering; its
counter advances before a rendering error and retains signed wrapping behavior.

An empty-description set has a retained compatibility quirk. The live reminder is
present with empty text, but the recorded change clears it, so reopening omits it.
A manually recorded description entry containing empty text still recovers as a
present reminder.

Reminder changes commit before publication. Publication failure or cancellation
does not roll back the committed change. A storage failure prevents publication.
A failed clear can leave the title absent in the current instance while reopening
restores the previously recorded reminder. That failure behavior is not an
implicit durable clear.

Direct callers must await or join reminder mutations. Dropping a mutation while
its database write is already admitted can commit the change without updating the
caller's live view or publishing it. Reopening restores recorded state. Runtime
mutation entry points retain accepted work through completion, but canceling only
the publication does not prove that an arbitrary dropped direct call is safe.

## Status and process inventories

Status observations use fresh snapshots, namespaced keys, deterministic key order,
and current extra providers. Blank or unavailable values are filtered. Provider
failures identify their key, and caller cancellation propagates. Status fitting
caps convergence at eight iterations.

The runtime tree starts at the queried agent, excludes disconnected ancestors and
siblings, and keeps idle ancestors of active descendants. Child nesting is
deterministic. Process display names derive from owner and name, while ordering
uses the opaque process identity. A missing scoped process observer is an error,
not an invented empty process inventory.

Active-process blockers include only nonretired processes that block a turn.
Process snapshots include all nonretired entries, including completed work whose
result has not been delivered. Observation must not retire or claim processes to
make an agent appear complete and does not replace process settlement or joins.

## File-based queues

The packaged queue skill coordinates files beneath the owner's
`$AGENT_SCRATCH_DIR/queues/` through ordinary authorized command execution and a
Python 3 helper. Workers receive explicit absolute paths. Owners refresh one
aggregate progress report using taken and total items, with a waiting count;
taken items may still be in flight. Active progress requires an explicit close.

There is no queue watcher, native queue inventory, or queue-specific completion
check. Old native queue files remain untouched and unattached. Historical protocol
and tool-result records remain readable. See
[file-based work queues](../../../../docs/operator/state-and-rollback.md#file-based-work-queues)
for stop and resume behavior.

## Capability limits

These contracts do not establish the production bridge for inline compaction or
real scoped process ownership. An injected process observer does not prove
operating-system liveness, filesystem authorization, or process cleanup. Known
blocked filesystem-security work remains unresolved. Full runtime, native-platform,
security, and release gates remain separate from component-level observations.
