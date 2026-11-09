# Session storage ownership

The user-facing change moves root identity, mode state, and permanent agent
lineage from SQLite into JSON. Database format 5 has 9 application tables instead
of format 1's 15. Root identity, mode state, and lineage no longer use the
`session_state` and `agent_lineage` tables. Tool calls, tool results, and context
reminders use conversation records instead of separate tables. There are no
automatic conversions.

## One metadata owner

`SessionIndex` owns the selected session's `meta.json`. The file contains the
stable root agent ID and name, current mode and checked revision, and an ordered
array of immutable agent identities. Existing model selection and preferences
remain in the same document.

The session lease shares one index between writers. Each mutation preserves
fields it does not own. Model-only publication cannot accept a mode value, so a
concurrent model callback cannot overwrite a newer mode. Whole-document
replacement stays inside the metadata owner.

Fresh opening publishes canonical identity before constructing agents or writing
their history. Resume requires valid identity metadata and the root recovery
snapshot. Missing metadata never triggers a replacement root ID. An interrupted
initial creation can therefore leave a session that refuses resume. The explicit
fresh-session path creates a different session without resetting the old one.

## Permanent identities and current recovery

History preparation records an identity before a child can commit its history
fork. Repeated registration of the same identity is idempotent. A conflicting
parent or name cannot replace an existing identity.

The ordered JSON array keeps parents before children and preserves successive
occupants of a name-based directory. Ordinary membership reads do not decode
historical event payloads. An identity prepared before failed agent construction
may remain without any execution or usage facts.

`FileAgentStateStore` continues to own the current `agent-state.json` recovery
snapshot for each agent directory. Retirement removes that snapshot, not the
permanent identity record. Lineage describes history membership, not which agents
to restore. The retained child roster determines recovery.

## Accounting without a root execution event

A restored child can have usage before the root has any execution facts. Runtime
restoration seeds the root's empty parent link from canonical JSON before replay
validates ancestry. It does not invent an execution event or usage. Facts still
supply descendant parent links, and missing non-root ancestors remain errors.

This input replaces the old SQL root-state fallback. Removing the fallback
without a JSON root seed failed existing nested-task recovery tests. The root
seed preserves those supported sessions without retaining a database table.

## Conversation-owned status consumption

The mode and its local revision live in JSON. A nullable
`conversation_item.mode_status_revision` marks the system message that consumed a
revision. A partial unique index prevents duplicate consumption for one agent and
revision. Ordinary messages and fork copies have no consumption marker.

A status append acquires the metadata owner's mode guard and checks the exact
expected mode and revision. The database transaction projects the message and
marks its consumed revision together. Its output retains the owned guard until
the database actor finishes commit. Dropping the caller's observer cannot release
the guard early and admit a mode change before that commit.

Mode updates use the same guard and advance the revision with checked arithmetic.
Other metadata mutations do not change mode or revision. A failed conversation
projection leaves the revision pending. A committed consumption marker survives
reopen and compaction. Derived `history.jsonl` refresh runs after commit and does
not determine whether consumption succeeded.

This is not a transaction between JSON and SQLite. A mode publication can succeed
before a later live-selection or recovery-descriptor update fails. Resume uses
the canonical JSON mode. Status consumption belongs only to the SQL transaction
that stores its conversation message.

## Alternatives and accepted cost

A dedicated `session-state.json` would separate operational state from listing
metadata, but it adds a second permanent document and duplicates mode and root
name in the listing. Extending the existing owner keeps one authority.

Permanent per-directory identity ledgers would avoid rewriting a session-wide
lineage array. They also require directory discovery and a separate permanent
lifetime beside removable recovery snapshots. The selected design avoids that
additional protocol. Its accepted cost is a growing metadata document and
whole-document replacement on mutation. This is a database-use reduction, not a
measured performance improvement.

## Format boundary and inspection

Formats 1, 2, 3, and 4, populated unversioned databases, and unknown formats use the
existing typed refusal path. Opening does not convert, reset, or delete them.
Current and legacy filesystem roots use the same check. `parrot chat --new-session`
selects a separate fresh session.

The [state and rollback guide](operator/state-and-rollback.md) explains operator
handling and the read-only `scripts/verify-session-storage` artifact check. Keep
complete session backups because identity metadata, recovery snapshots, and the
database have different responsibilities.
