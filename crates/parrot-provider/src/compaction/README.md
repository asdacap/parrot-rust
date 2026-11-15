# Compaction and context budgets

Compaction replaces an eligible conversation prefix while retaining recent history
and incomplete tool-call groups. The provider owns selection, summarization,
truncation, fitting, and lifecycle publication. Core requests one awaited operation
through `Provider::compact` and receives a finalized replacement message or a
successful no-op. Runtime supplies raw conversation reads, status offerings,
persistence, and event delivery without driving compaction phases.

A replacement message can carry summary text or opaque provider-native replay
content. The current generic implementation produces text summaries. It rejects
an opaque-only prior replacement rather than inventing a text conversion. Native
compaction endpoints are not implemented here.

Summarized history remains in the authoritative database. Retained messages
preserve tool identities, results, image references, and provider-native replay
content.

## Budgets and triggers

Context-window reporting and input-limit triggering are distinct. Estimates include
text, tools, selected-provider image costs, and fixed request overhead without
loading image bytes. Calibration uses positive observed input usage for the selected
model, preserves its previous value when usage is missing, and resets after
successful finalization, before terminal completion publication. Calibrated
estimates saturate. Aggregate token overflow is an error. Output budgets retain
five-percent headroom.

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

Selection respects the existing summary watermark and the earliest incomplete
tool-call group in the selected prefix.
Assistant calls and their following tool results remain causal groups. A group is
complete when every tool-call identity has a result, without requiring equal call
and result counts. Orphan messages stay separate. Blank tool-call identities are
rejected.

Complete groups can be chunked for summary requests. An empty summary causes
recursive folding into smaller chunks rather than silently dropping history.

Oversized groups are truncated in copied summary input, not written to files.
Text prefixes end at Unicode boundaries and omissions use the configurable
`compaction.truncated-group-notice` template. Images can be omitted from these
copies. Roles, tool-call identities, tool names, argument JSON, and result links
remain intact. Native replay content is cleared only in truncated copies so it
cannot override the clipped text. Durable source messages and retained history
are unchanged.

Every summary request is checked against its complete input budget, including
instructions, the actual preceding summary, and omission notices. A preceding
summary can require further truncation of a later group. If fixed metadata or
request overhead cannot fit, compaction returns an explicit error.

The final result must fit the selected model's complete input limit, including
fixed status and retained groups. Status fitting compares complete context
snapshots and allows at most eight rerenders after its initial rendering. A lack
of a safe boundary or a final input-limit failure is reported instead of recording
a plausible replacement.

## Persistence, cancellation, and progress

Automatic, oversized-request, standalone, and inline compaction use the same
selection, summary, and fitting behavior. Prompt inputs are refreshed before
rendering the new epoch. The previous summary and its post-watermark durable tail
are read during a serialized epoch. Core assembles provider-visible messages from
the full summary message, its associated status row, and the tail without older
compaction statuses. The provider groups the raw history for compaction. Native
tail reads retain tool ownership and status classification and materialize
historical image artifacts.

Compaction awaits summary requests and admitted cleanup. Cancellation reaches
summaries and other compaction effects. Started, finished, and failed notices
bracket the operation, including no-op completion. Preparation failure after a
successful start publishes failure before returning without recording a
replacement.

The provider initiates started, aggregate progress, saving, and terminal events
through the event sink. Progress counts summary requests, including recursive
fallback requests. Saving precedes commit. Finished follows successful
finalization. A successful no-op still completes its lifecycle without saving.
Summary text is not published as progress. Provider retry notices remain visible.

Finalization stores the complete replacement message, associated status, and
watermark, reloads the live conversation, and publishes the committed status
receipt. Calibration resets after those effects succeed and before finished is
published. A failed finished publication therefore preserves both the committed
replacement and the reset. A failed committed-status publication preserves the
replacement but does not reset calibration. Errors after commit do not imply
rollback.

Storage retains replacement role, message parts, and opaque provider fields.
Existing text-only snapshots remain readable. Full forks copy the replacement
with a remapped watermark. Named forks retain checkpoint-only history and do not
copy the earlier replacement.

Publication failures can replace an earlier reported error. Retained status,
reminder, final-phase, and restoration notices are not themselves proof of a
durable event record.

## Ordering and errors

Core retains compaction trigger timing, including waiting for the entire tool
batch and its results before tool-requested compaction. External compaction is
serialized with agent work and waits for an active provider.
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
