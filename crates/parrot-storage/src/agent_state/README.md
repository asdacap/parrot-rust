# Agent-local recovery snapshots

`FileAgentStateStore` persists one versioned `agent-state.json` in an existing private agent directory. It does not construct agents, activate tasks, or recover conversations. The domain `AgentStateStore` trait is the boundary used by composition and runtime consumers.

Create one store per agent scope and share it between section owners. `initialize` only accepts an absent snapshot. `load` returns `None` for absence and distinct errors for corrupt data, unsupported versions, and another owner's state. `update` holds the scoped gate across read, section replacement, revision increment, and atomic write. Section updates preserve other sections. Callers updating the same section must coordinate their own transitions.

Operations return `BoxFuture` and require a Tokio runtime. Once polled, an operation acquires the scoped async gate and submits a blocking worker. That submission admits the operation. Dropping the observer before admission prevents it from starting. After admission, the worker owns the gate until the operation finishes, even if its observer is dropped. Filesystem I/O and serialization run on blocking workers. No operation starts an agent execution or emits a notification.

The typed snapshot contains the owner's reconstruction descriptor, references to retained direct children, full task graphs and runner checkpoints, and correlations between executions, results, and notifications. Each child's snapshot belongs in that child's own directory. A parent stores references, never copies of descendant snapshots. History message IDs and sequence numbers refer to durable conversation history rather than duplicating conversation bodies.

A write uses `agent-state.json.tmp` in the same directory, syncs its contents, and renames it over the destination. The next write discards stale staging files rather than treating them as committed snapshots. `retire` requires the caller to stop and join all writers first. It validates the current snapshot's schema and owner, then removes only the snapshot and staging basenames. On Unix it syncs the containing directory after unlinking. The shared store becomes retired even if cleanup reports an error, so it cannot recreate state. An absent snapshot then makes retry safe. A pre-commit failure leaves the previous snapshot intact. On Unix, the store opens the directory one component at a time without following links. All operations use its pinned directory descriptor. The snapshot is private, and the store syncs the containing directory after the rename. A directory-sync error after rename means the new snapshot may already be visible. Callers must treat that error as uncertain durability, not retry by launching unrecorded work. Other platforms use their existing path and link validation conventions, and they do not claim Unix descriptor-level race confinement or directory-sync durability.

The file and staging basename are reserved against child-agent names. Existing data is never replaced merely because decoding failed. There is no cross-process writer coordination. The selected-session owner and the single scoped store remain responsible for exclusive lifecycle ownership.

## Location and authority

Composition opens the store through `AgentStateFiles` using the selected session
root and the owning agent's validated name path:
`<selected-session>/root-agents/<root-name>/<child-name>/.../agent-state.json`.
`load` checks the expected agent-session ID, so a name-based directory from
another incarnation cannot silently supply its state. Child roster references
contain direct names and IDs. A referenced child's descriptor must match that
parent. Unreferenced directories are not a recovery inventory. The staging file
remains beside its owner's final file. There is no session-wide snapshot registry
or shared descendant snapshot.

SQLite history for conversations and events remains authoritative for admitted
inputs, responses, and tool settlements. `history.jsonl` remains a projection. It
is not loaded to rebuild a graph. Existing queue files remain in the owning
agent's `root-agents/<root-name>/<child-name>/.../queues` directory beneath the
selected session, not in the snapshot. Descriptors retain profile and model
selectors, local scope, and completion mode. They do not retain credentials or
security grants. Runtime resolves selectors and rebuilds parent-linked
restrictions from current configuration.

## Cross-store recovery

This file is an internal recovery format. Its task payload serialization is not
the `set_agent_tasks` input artifact format. Runtime consumers save stable intent
before prompt admission and reconcile it against SQLite using input IDs and
response references, rather than replaying all history. Terminal execution state
also retains a recovery watermark distinct from its exact assistant response
sequence, so later genuine input is not swallowed by an older terminal result.
Inherited fork context alone is not destination-owned pending input.

Child construction writes the child's descriptor before adding its reference to
the parent roster. Writes of scheduler outcomes, checkpoints, and notification
outbox entries are ordered around durable conversation admission and consumption,
but atomic file replacement is **not** a transaction with SQLite. Reopening must
handle either side having committed. In particular, unresolved tool recovery can
repeat an external effect if death occurs after that effect and before the
terminal settlement commits. Exactly-once external effects are not promised.

For an existing user session with no root snapshot, host composition warns and
uses root-only legacy recovery, then starts writing new state. It does not infer
old child selectors or task graphs from events. Missing **referenced child**
snapshots, corrupt files, files of an incompatible version, and owner mismatches
instead fail opening. Operators should preserve them for diagnosis, not delete
them to force an empty recovery. See the
[operator guidance](../../../../docs/operator/state-and-rollback.md#retained-agents-and-task-recovery).
