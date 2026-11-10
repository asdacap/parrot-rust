# Task artifacts

`set_agent_tasks` input supplies either a file path or inline tasks, never both.
File input must resolve to an authorized regular file without symbolic links.
Malformed input, denied or failed reads, parse errors, and cancellation prevent
dispatch. Parsing alone does not accept an update. Dispatch must also succeed
before the operation returns the accepted task count.

## File decoding

File input accepts UTF-8, including an optional byte-order mark. Byte-order marks
also select UTF-16 little-endian, UTF-16 big-endian, UTF-32 little-endian, or UTF-32
big-endian decoding. Invalid UTF-8 sequences use replacement characters. There is
no additional artifact byte limit at this boundary.

## Dispatch and validation limits

Dispatch preserves the originating assistant sequence, tool-call ID, and captured
selection. Rejected artifacts never reach dispatch.

Parser and dispatch checks do not establish production filesystem-security or task
scheduler correctness. Host integration and filesystem-security validation remain
separate requirements. A successful parser check is not full application or
release acceptance.

See [assignment and accepted-response recovery](../../../../docs/operator/state-and-rollback.md#assignment-and-accepted-response-recovery)
for task assignment, completion, and accepted-response replay behavior.
