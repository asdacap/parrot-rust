# Session storage ownership

A session keeps its root identity, mode, preferences, and permanent agent lineage
in `meta.json`. SQLite database format 9 stores conversation history, including
tool calls and results, context reminders, named checkpoints, compaction summaries,
and image metadata. It has 5 application tables. Earlier database formats are not
automatically converted.

Named checkpoints require a `set_checkpoint` call with valid title arguments and
a completed successful result. Forking and compaction use the latest completed
checkpoint for each title. Unexecuted, failed, cancelled, and malformed calls do
not create checkpoints. History forks preserve checkpoints.

Each compaction summary identifies the last conversation item it covers, separately
from where the summary appears in history. Usage history refers to conversation
sequence numbers. `history.jsonl` is a derived view, not a recovery authority.

## Image metadata belongs to its part

Each image part retains its inspected media type, byte length, dimensions, frame
count, aggregate pixels, display name, and origin. Promotion, history forks, JSON
history, and execution recovery preserve that metadata.

The selected session's `artifacts` directory stores image bytes under their
lowercase SHA-256 hash, without a filename extension. Identical bytes share one
file. Message admission verifies the local bytes, hash, and image limits rather
than trusting client-supplied dimensions, media type, or length.

Uploads have no separate identity or database row. Message image references carry
their content hash and display name. Upload responses and tool reads retain each
reference's own display name and origin. Identical bytes can therefore appear
under different names without duplicating the image file or sharing a first label.
The S3 provider configuration and credential name `image_upload` are unchanged.

Session opening removes expired image files only when neither input nor
conversation parts refer to them. File modification time supplies the age,
including completed uploads that were never attached to a message.

## One metadata owner

The selected session's `meta.json` contains the stable root agent ID and name,
current mode and revision, and an ordered array of immutable agent identities.
Model selection and preferences remain in the same document.

Metadata updates preserve unrelated fields. A concurrent model-selection update
cannot overwrite a newer mode. Mode revisions advance without wrapping on overflow.
Other metadata updates do not change the mode or its revision.

Fresh sessions record their identity before agent history. Resume requires valid
identity metadata and the root recovery snapshot. Missing metadata never triggers
a replacement root ID. Interrupted initial creation can therefore leave a session
that refuses resume. Explicit fresh-session creation makes a different session
without resetting the old one.

## Permanent identities and current recovery

A child's identity is recorded before its forked history. Repeated registration
of the same identity is harmless. A conflicting parent or name cannot replace an
existing identity.

Lineage keeps parents before children and preserves successive occupants of a
name-based directory. An identity recorded before failed agent construction may
remain without execution or usage facts.

Each agent directory has its own current `agent-state.json` recovery snapshot.
Retirement removes that snapshot, not the permanent identity record. Lineage
describes historical membership, not which agents to restore. The retained child
roster determines recovery.

## Accounting without a root execution event

A restored child can have usage before the root has any execution facts. Such
sessions still recover their root ancestry from `meta.json`, without inventing a
root execution event or usage. Missing non-root ancestors remain errors.

## Conversation-owned status consumption

A status message consumes one mode revision for one agent. The message and its
consumed revision commit together, and ordinary messages and fork copies do not
consume revisions.

Status consumption requires the expected mode and revision to remain current.
A concurrent mode change cannot take effect during that commit. Dropping the
caller's observation does not weaken this ordering.

A failed conversation write leaves the revision pending. A committed consumption
survives reopening and compaction without a second status message for that revision.
Refreshing the derived `history.jsonl` happens after commit and does not determine
whether consumption succeeded.

JSON metadata and SQLite do not form one transaction. A mode update can persist
before a later live-selection or recovery-snapshot update fails. Resume uses the
mode saved in `meta.json`.

## Alternatives and accepted cost

A dedicated `session-state.json` would separate operational state from listing
metadata, but it adds a second permanent document and duplicates mode and root
name in the listing. Keeping them in `meta.json` avoids a second authority.

Permanent per-directory identity ledgers would avoid rewriting a session-wide
lineage array. They would also require directory discovery and a permanent
lifetime separate from removable recovery snapshots. The selected design instead
accepts a growing metadata document and whole-document replacement on mutation.
This reduces database use. It is not a measured performance improvement.

## Format boundary and inspection

Formats 1 through 8, populated unversioned databases, and unknown formats fail to
open. Opening does not convert, reset, or delete them. Current and legacy
filesystem roots use the same check. `parrot chat --new-session` selects a
separate fresh session.

The [state and rollback guide](operator/state-and-rollback.md) explains operator
handling and the read-only `scripts/verify-session-storage` artifact check. Keep
complete session backups because metadata, recovery snapshots, and the database
have different responsibilities.
