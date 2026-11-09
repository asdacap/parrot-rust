---
name: reflect
description: Spawn three parallel review subagents over the active transcript, surface learnings, and route each to a concrete edit on an existing skill. Use when the user says reflect.
---

# Reflect

Mine the current conversation for durable learnings, then route them into skill edits.

Invoke when the user says "reflect" or "/reflect". Skip when the conversation is trivial, off-topic, or already covered by an existing skill the parent followed correctly. One-offs are not learnings.

## 1. Locate the active transcript

Before fanning out, use only the current session's explicitly supplied `AGENT_DIR/history.jsonl` transcript. Do not search unrelated sessions or home directories. If it is unavailable, pass a tight digest of the session instead.

## 2. Spawn three reviewers in parallel

One message, three `agent_spawn` calls with `agent: worker`. Reviewers need access to connected MCP tools to look up the tickets, chat threads, and observability traces the transcript references. They remain read-only: do not write files or mutate external state.

Choose available configured models for each reviewer and the synthesizer according to the configured delegation policy and the Subagents section of the **Behavior guideline** system prompt.

| Lens | Prompt template |
|---|---|
| Judgment | `references/judgment-reviewer.md` |
| Tooling | `references/tooling-reviewer.md` |
| Divergent | `references/divergent-reviewer.md` |

Pass each template verbatim, substituting the transcript path or digest where marked. Reviewers return findings in their `agent_spawn` completion results.

## 3. Synthesize

One `agent_spawn` call with `agent: worker` and an available configured model for the synthesizer role. It spot-verifies citations through connected MCP tools without writing files or mutating external state. Pass `references/synthesizer.md` verbatim, with each reviewer's full output inlined where marked. It returns an Accepted / Rejected / Backlog list.

## 4. Structural enforcement check

Move any Accepted item that a lint rule, script, metadata flag, or runtime check would enforce more reliably to Backlog. See **Encode Lessons in Structure** in the **Behavior guideline** system prompt.

## 5. Apply

Present the synthesizer's full Accepted / Rejected / Backlog output and wait for explicit approval before applying any Accepted edit. The user picks the subset and may redirect routings. Skill changes affect every future agent in the org. Do not auto-apply.

File each Backlog item to your team's devex or backlog tracker only when that external action is authorized. Otherwise retain a local actionable backlog. The Accepted list waits for approval.

Never edit an installed skill cache. Before approval, show the editable skill source path supplied by the operator or environment. After approval, edit that source or copy the skill folder to the approved destination and edit the copy. Never open a PR or push.

Follow each approved row's Routing exactly:

- Trivial existing-skill edit (a one-line bullet, a tightened sentence, a stale fact corrected): the parent does it directly.
- Substantive existing-skill edit (a new section, a new pattern table, more than ~10 lines): hand to the **skill-creator** skill and run its draft / test / iterate loop.
- `tune description: <skill path>` (the skill exists but didn't trigger when it should have): hand to **skill-creator** and run its validation and iteration workflow.
- `new skill via skill-creator: <kebab-name>`: hand creation to **skill-creator**. Do not invent the shape ad hoc.

If your environment ships a SKILL.md validator, run it on every touched skill before declaring done.

## 6. Summarize for the user

Short list, no preamble:

- Edits applied: `<skill path>`. What changed, one line each.
- New skills created: `<skill path>`. One line each (rare).
- Backlog filed to the devex tracker or retained locally: `<issue title>` (`<tags>`). One line each.
- Dropped: one line per rejected finding + reason from the synthesizer.
