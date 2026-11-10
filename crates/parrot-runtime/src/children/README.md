# Retained child agents

A child retains its identity, history, local prompt scope, and completion-delivery
mode after its foreground execution finishes. Finishing a parent turn does not
retire the child. New pending input can wake a retained completed child.

## Selection, naming, and paths

Each turn applies the currently selected model and profile together with current
ancestor restrictions. A selection captured for an existing turn does not change
when later selections change. Losing a parent from the live topology does not
remove inherited policy restrictions. Prompt descriptions are not security grants.
Profile comparison is case-sensitive.

Child names use ASCII sanitation and numeric conflict suffixes starting at `-2`.
An empty sanitized name uses the final six identity characters. Reserved names
cannot become child directories. Maximum child depth is four.

Reusing a named child preserves its identity, history, services, and prompt scope
while updating its current model, profile, and security selection. Getting an
existing direct child does not evaluate a fresh-launch callback. A busy child
still fails the normal busy check.

Status lookup treats bare names as direct children only. Recipient lookup can
select a parent through `parent` or the parent's actual name. Relative paths
support `..` and `parent`. Absolute paths require one uniquely named root. Empty
and `.` path segments are rejected. An explicit resource prefix never falls back
to a different owner. Resolving a path does not grant access to that resource.

Sibling-history forks require a registered sibling of the same parent. History
forks and named checkpoints preserve the selected safe history boundary rather
than creating a separate conversation-selection policy.

## Admission and close

Root admission rejects duplicate identities, a duplicate live session, or a root
with a parent or nonzero depth. Child admission enforces the owning parent and
normal policy limits. Failed publication startup leaves admission retryable while
preserving cleanup of work already started.

Admission order determines child snapshot order. Closing disposes children in
reverse order and hides live snapshots immediately. Already admitted construction
and rejected-construction cleanup are joined before shutdown finishes. A failed
construction does not announce a successfully created or closed child.

Close joins owned work, closes the session and child admission, disposes retained
children and registered resources, releases retention, and joins started
publications. Cleanup continues through failures. Later cleanup stages can replace
an earlier reported error; within an ordered cleanup list, the first failure is
retained. Additional failures produce bounded diagnostic codes rather than
exception, prompt, profile, or policy text.

## Completion delivery

Ordinary child completion sends the result to the parent as a steer and waits for
send admission. Delivery failure does not retire the child. Retained-only
completion does not send a parent message. Result fields and final text have
rune-safe 1 MiB bounds.

## Recovery

Each agent's `agent-state.json` lives under its own directory in the selected
session. A parent retains direct-child names and identities, not copies of child
snapshots or conversation history. The snapshot and its staging basename are
reserved against child names. See the
[operator recovery guide](../../../../docs/operator/state-and-rollback.md#retained-agents-and-task-recovery).

A fresh child's descriptor is saved before its parent records successful
admission. Recovery follows that saved roster, parent first. Old lineage events
and leftover directories do not recreate obsolete children. Existing identities,
histories, scopes, and completion modes are reused without another fork. Idle,
completed, and never-started children remain retained, but reopening them alone
does not request a provider response.

Saved model and profile selectors use current configuration and normal ancestor,
profile, and depth restrictions. Missing selectors are errors, not permission to
choose substitutes. Recovery restores neither credentials nor old live grants.
A recovered task worker has one execution owner, so reopening does not submit the
same unfinished task independently through both task and ordinary recovery.

Normal hosted close stops new construction and task admission, suspends unfinished
sessions and graphs, and joins accepted work before canceling shared resources.
Explicit interruption, task cancellation, and reset remain distinct from
suspension. Old native queue files remain untouched and unattached. Shared
file-based queues use the packaged queue skill and explicit paths under their
owner's scratch directory.

Resuming a current-format session requires its root snapshot. Missing referenced
child snapshots, corrupt or unsupported snapshots, owner or parent mismatches,
duplicate retained names, and cyclic identities stop opening before provider
activation. Preserve the session for diagnosis rather than deleting files to force
an empty recovery.

Snapshot files and SQLite do not form one transaction. Stable execution, input,
and completion identities reconcile their durable boundaries. Recovery does not
roll back external effects or guarantee exactly-once execution. Historical tool
calls without saved results receive crash errors rather than automatic execution.

These child-lifetime contracts do not establish production host, filesystem
security, native-platform, or release acceptance. Known blocked filesystem-security
work remains unresolved by component-level checks.
