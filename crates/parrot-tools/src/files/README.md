# File tools

`read`, `read_image`, and `glob` permit parallel execution. `write` and `edit`
execute sequentially. These tools are not enabled after interruption. File access
uses the invocation's captured security policy and cancellation. Paths and
metadata do not grant access.

## Paths and reads

`read`, `write`, and `edit` expand captured path variables once, using case-sensitive
ordinal matching. Unknown variables remain literal. `read_image` and `glob` pass
path arguments unchanged to workspace-relative resolution.

Reads remain bound to the expected physical path and captured security policy.
Directory entries are re-inspected, and entries that cannot be read are filtered
where the operation permits. A fallback read must still be authorized.

Text reads probe the first 8192 bytes for NUL and support byte-order-mark-aware
decoding with invalid-sequence replacement. Line counting recognizes CR and LF.
Requested ranges are limited to 2000 lines, and text output is limited to 8192
UTF-8 bytes. Results include the total line count and a partial Markdown outline.
Directory-output limits count UTF-16 code units. The final appended entry can
exceed the prechecked directory-output limit.

## Images and search

Each image result retains its own display name and origin. Image bytes are stored
before the per-cycle budget accepts the result, even for duplicate content. An
already exhausted budget rejects the request before argument parsing or file I/O.
Template formatting does not observe cancellation, but file I/O does.

Glob results use ordinal UTF-16 ordering and depth-first traversal. Directory
links can be reported but are not followed recursively. Search stops at 1000
results, 100000 visited entries, or its five-second deadline.

## Writes and edits

Both operations authorize the original request under write policy before reading
existing bytes. Changed output requires a second authorization of that original
request under the same captured policy. Equal bytes cause no mutation.

`write` can create files and parent directories. `edit` does not request their
creation. Write results report before and after logical line counts, including
unchanged content. Edit diffs use the final display path of the successful write.

Edits match case-sensitively in ordinal order without overlapping matches. The
default requires exactly one match. Replace-all permits zero matches as a no-op.
Edits require UTF-8, preserve an existing UTF-8 byte-order mark, and leave CR and
LF bytes unchanged. Writes emit UTF-8 without a byte-order mark. Diffs include
three context lines and can use a bounded fallback for large comparisons.

## Security qualification

Tool orchestration checks do not establish production filesystem-security
correctness. Validation remains incomplete for physical file sharing, actual-target
and symlink authorization, Unix modes, timestamp preservation, safe atomic
mutation, and per-agent session-root isolation.

The previously recorded captured-file approval-widening failure remains unresolved.
No unrestricted fallback is permitted. These tool contracts are not full security,
application, or release acceptance.
