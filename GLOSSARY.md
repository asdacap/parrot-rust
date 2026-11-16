# Parrot workspace

Workspace identity and conversation terminology used by Parrot.

## Language

**Project workspace**:
A project's launch location together with its resolved identity, repository information and candidate write-scope roots. The project workspace does not itself confer permission.
_Avoid_: Session root, current directory

## Conversation language

**Context**:
An LLM's conversation history. The term is reserved for this meaning, not general application state or repository information.

**Conversation turn**:
A user's input and the response messages and tool activity that follow it. A compacted conversation view may retain only part of the turn.

**Open turn**:
A conversation turn that has begun but has no recorded terminal outcome in the agent session's event stream. A session may hold several open turns after a crash; close-turn arbitration targets the latest one.
_Avoid_: Active turn, current turn

**Turn result**:
A no-tool assistant response at the end of the visible conversation turn, which may be a completion candidate. It does not by itself mean the turn has been accepted as complete.

**Step**:
One provider request and its response within a conversation turn. A turn spans many steps. The same unit as an ATIF Step and a DeepSWE API turn.
_Avoid_: Tool-call-cycle, API turn

**Pending message**:
Input accepted for delivery but not yet admitted into the conversation.

**Steer**:
A pending message eligible to join the turn already being processed.

**Queued message**:
A pending message intended to start a new conversation turn rather than steer one already being processed.
