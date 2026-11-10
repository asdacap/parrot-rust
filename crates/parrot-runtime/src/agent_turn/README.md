# Agent turns and recovery

An agent processes admitted input in order. A turn uses the selected model,
profile, security policy, and executable tool catalog captured for that turn.
Changes to selection apply to later turns. Tool descriptions do not confer
execution authority.

## Input admission and turn ordering

Input is durable before listeners receive its admission notice. Resending an
already admitted input does not duplicate it, but can wake pending work.

Steers join the current conversation together. A queued message starts its own
turn when no steer is ready and the previous conversation no longer needs a
response. Prompt preparation, status, skill context, and tool-restoration notices
precede the provider request that uses them. Selected skill bodies are loaded
once per stable skill identity within a turn.

An idle agent does not initialize a provider turn merely to check for input.
Incoming-input waits preserve their registration until delivery, cancellation,
or disposal. Queued operations retain admission order even when their callers
stop observing them.

## Requests and tools

Provider-request limits determine entry into the final phase. Tool settlement can
grant at most four bounded request extensions. Exhausting either limit ends the
turn with the configured failure instead of continuing without a bound.

A truncated tool-call response or invalid JSON arguments are rejected before
recording or executing the tool batch. JSON arguments may be any valid JSON
value. The assistant's accepted tool calls are recorded before execution starts.
Each execution group settles durably before the next group starts, so later tools
can observe earlier results.

Execution uses the captured executable tools rather than reconstructing them from
advertised definitions. Tool cancellation is awaited, including cleanup of work
already admitted. Recording an assistant tool call alone does not execute it.

Final-phase and tool-restoration notices are idempotent. Repeating a notice that
is already reflected in the conversation adds neither a message nor a publication.

## Tool results after interruption or process death

Historical tool calls are never scheduled for execution again. Saved tool results
are restored unchanged. A historical call without a saved result receives the
configured crash error, whose default text is `The process crashed before the tool
completed.` The error is saved before publication. Repeating recovery does not
replace completed results or republish their terminal notices.

Interrupted or failed batches can therefore leave crash errors when conversation
processing resumes. External effects may already have happened before the missing
result. Neither a crash error nor recovery rolls those effects back or establishes
exactly-once external execution.

## Compaction during a turn

External compaction waits for the active provider and shares admission order with
other agent operations. Cancelling a queued caller does not cancel unrelated
work. Shutdown cancels queued compaction and rejects later requests without
starting a provider or emitting compaction events. Cancelling a running compaction
does not cancel the next compaction already admitted.

A tool can compact the conversation within its current turn. That summary becomes
available to the next provider request in the same turn. Such an operation must
not enqueue work on the same agent and wait for itself. Cancellation waits for
summary cleanup before the turn finishes.

## Completion and durable state

A no-tool assistant response is a completion candidate, not proof that the turn
has closed. Completion observes the current reminders and other blockers,
including changes accepted after agent construction. Any nonempty blocker list
retains the candidate and forces continuation. Accepted reminder changes finish
before final closure is decided.

A turn ending commits before its publication. Cancellation or publication failure
after that commit does not append a second ending or assistant response. A
repeated close observes the existing durable ending. A failure before admission
can still require interruption cleanup.

Normal shutdown joins admitted mutations, provider streams, provider sessions,
and output cleanup. Dropping an operation's observer does not release the agent
while accepted work remains unfinished. A mutation's reported failure belongs to
that operation; an unexpected task failure also remains a cleanup error.

## Streaming, usage, and diagnostics

Text and reasoning fragments are buffered, with a held suffix flushed after
500 milliseconds even if the next stream read stalls. Retries flush the prior
phase. Empty data does not clear headers. Stream errors and cancellation still
await buffered-output cleanup and stream close.

Recorded request and tool facts determine recovered usage. Own, descendant,
cumulative, and context usage remain distinct. Live aggregate usage notifications
are not durable history facts. Usage already recorded is not rolled back when a
later stream or publication fails.

Operational diagnostics contain bounded outcomes and error codes, not prompt or
security-policy bodies. A provider failure uses only its top-level response body,
not arbitrary nested causes. Cancellation remains distinguishable from failure.
A started turn has a matching diagnostic finish, followed by parent delivery.

## Recovery limits

If the process dies after a turn ending commits but before publication, the ending
remains authoritative. Pending queued input can recover without repeating the
completed response or its usage. Derived `history.jsonl` may lag the committed
database at that cut and refresh during subsequent history recovery.

That recovery case does not establish startup-only projection repair without
follow-up work, power-loss durability, or correctness at every possible crash
boundary. Runtime component checks do not by themselves establish production tool
bridges, filesystem authorization, native-platform support, or release acceptance.
Known blocked filesystem-security work is not waived by these guarantees.
