# Compaction and context budgets

Compaction replaces an eligible conversation prefix with summaries while retaining
recent history, protected checkpoints, and incomplete tool-call groups. Summarized
history remains in the authoritative database. Retained messages preserve tool
identities, results, image references, and provider-native replay content.

## Budgets and triggers

Context-window reporting and input-limit triggering are distinct. Estimates include
text, tools, selected-provider image costs, and fixed request overhead without
loading image bytes. Calibration uses positive observed input usage for the selected
model, preserves its previous value when usage is missing, and resets only after
successful compaction persistence. Calibrated estimates saturate; aggregate token
overflow is an error. Output budgets retain five-percent headroom.

Context reminders compare current usage with the last stored reminder for the
same model and context limit. They use 10-percentage-point bands starting at 50%.
Committed compaction clears the stored reminder checkpoint. A reminder that would
cross the compaction trigger is discarded in favor of compaction. A reminder's
token cost is checked before insertion, and only a newly inserted reminder is
published. Once inserted, its publication is not canceled with the request.

Forced compaction bypasses the ordinary trigger but leaves context unchanged
when the input limit is unavailable. A direct context-epoch operation can still
fail its no-progress comparison for zero or negative input capacity. Unavailable
capacity is not a guarantee that every compaction entry point is a no-op.

## History selection and summaries

Selection respects the active checkpoint boundary, the existing summary
watermark, and the earliest incomplete tool-call group in the selected prefix.
Assistant calls and their following tool results remain causal groups. A group is
complete when every tool-call identity has a result, without requiring equal call
and result counts. Orphan messages stay separate. Blank tool-call identities are
rejected.

History-only planning can use positional watermarks. Those positions are not
durable repository sequence numbers or evidence that history has committed.

Complete groups can be chunked for summary requests. An empty summary causes
recursive folding into smaller chunks rather than silently dropping history.
Oversized eligible groups can spill to blobs. Decisions use actual preceding
summaries and actual persisted-path notices, including their token cost. Ordered
first-pass spills and the once-only late spill remain distinct.

The final result must fit the selected model's complete input limit, including
fixed status and retained groups. Status fitting compares complete context
snapshots and allows at most eight rerenders after its initial rendering. A lack
of a safe boundary or a final input-limit failure is reported instead of recording
a plausible replacement.

## Persistence, cancellation, and progress

Automatic, oversized-request, standalone, and inline compaction use the same
selection, summary, and fitting behavior. Prompt inputs are refreshed before
rendering the new epoch. The durable history snapshot includes the previous
summary, its post-watermark tail, tool ownership, status classification, and active
checkpoints. Current transient messages remain separate.

Summary requests and admitted cleanup are awaited. Cancellation reaches summaries
and other compaction effects. Started, finished, and failed notices bracket the
operation, including no-op completion. Preparation failure after a successful
start publishes failure before returning without recording a replacement.

Progress reports completed and total conversation groups after successful folds,
including recursive fallback folds, and reports a saving phase before commit.
Summary text is not published as progress. Provider retry notices remain visible.

A successful commit stores the fitted snapshot, status, and watermark, and the
conversation is reloaded. Calibration resets only after successful persistence.
Publication failure can occur after persistence and can replace an earlier
reported error. Retained status, reminder, final-phase, and restoration notices
are not themselves proof of a durable event record.

## Ordering and errors

External compaction is serialized with agent work and waits for an active provider.
Shutdown cancels queued work and rejects later admission. Canceling one running
compaction does not cancel the next already queued request. Inline tools compact
within their existing turn without queuing an operation that waits for itself.

Typed errors distinguish invalid-operation conditions, invalid arguments, and
opaque template or service failures. Error text must not be parsed to infer a
diagnostic category. Unexpected service failures are not all invalid operations.

## Capability limits

Compaction behavior does not establish production tool routing, session-wide
serialization, filesystem authorization, or exactly-once external effects by
itself. Known blocked captured-file approval widening remains unresolved by
component checks. Security verification, native-platform gates, and release
acceptance remain separate from compaction's storage, template, and provider
contracts.
