---
name: wayfinder
description: Plan a huge chunk of work (more than one agent session can hold) as a shared map of decision tickets kept as agent tasks in the agent scratch directory, and resolve them one at a time until the way to the destination is clear.
---

Use this skill for work that is too large for one agent session and still has unresolved planning decisions. Record the goal and decisions in a **shared map** in the agent scratch directory. Resolve its **decision tickets** one at a time until you can plan how to reach the **destination**. Each decision ticket asks a question whose answer settles a decision, not an implementation task.

The destination is the goal of the effort. Define it before you create tickets because it determines their scope. The goal can be a specification, a decision needed before planning, or a change such as a data-structure migration. Use the map for engineering, course content, or other work with unresolved decisions.

## Plan, don't do

By default, Wayfinder resolves **planning** decisions, not implementation tasks. The map is complete when no decisions remain before execution. When you can start implementation without further decisions, hand off the map. An effort can explicitly include execution in its **Notes**. Without that override, produce decisions, not deliverables.

## Refer by name

Every ticket is an agent task, so it has a **name**. In everything the human reads (narration, the map's Decisions-so-far), refer to it by that name, never by a file path or slug. A name wraps its ticket file link; the path rides _inside_ the name, never stands in for it.

## The tracker

The tracker is a directory of files in the agent scratch directory, one directory per map:

```
$AGENT_SCRATCH_DIR/wayfinder/<map-slug>/
  map.md              the map body
  tickets.json        every ticket, as an AgentTask v1 artifact
  tickets/<slug>.md   a ticket's claim and resolution, absent while it is open and unclaimed
  assets/             prototypes, research findings, and anything else a ticket links
```

A slug is the name lowercased, with every run of characters other than `a-z` and `0-9` replaced by a single `-`. Always hand other agents the absolute map directory path; `$AGENT_SCRATCH_DIR` is different for every agent.

### The map

`map.md` is the canonical artifact. The map is an **index**, not a store. It lists the decisions made and points at the tickets that hold their detail; a decision lives in exactly one place, its ticket, so the map never restates it, only gists it and links.

The whole map at low resolution, loaded once per session. Open tickets are **not** listed: they are the tickets in `tickets.json` without a closed ticket file.

```markdown
# <map name>

## Destination

<what reaching the end of this map looks like: the spec, decision, or change this effort is finding its way to. One or two lines; every session orients to it before choosing a ticket.>

## Notes

<domain; skills every session should consult; standing preferences for this effort>

## Decisions so far

<!-- the index: one line per closed ticket, enough to judge relevance, then zoom the link for the detail the ticket holds -->

- [<closed ticket name>](tickets/<slug>.md): <one-line gist of the answer>

## Not yet specified

<!-- see "Fog of war": in-scope fog you can't ticket yet; graduates as the frontier advances -->

## Out of scope

<!-- see "Out of scope": work ruled beyond the destination; closed, never graduates -->
```

### Tickets

`tickets.json` is a strict AgentTask v1 artifact, `{"schema_version":1,"tasks":[...]}`, holding one top-level task per ticket, open or closed. It is a valid input to `set_agent_tasks`, so any unblocked ticket can be handed to a subagent as is. Keep it flat: every payload is a string, never a nested task array. Each task is sized to one 100K token agent session:

```json
{
  "name": "<ticket name>",
  "description": "wayfinder:<type>, <HITL|AFK>. <the question in one line>",
  "payload": "Work this ticket with the `$wayfinder` skill on the map at <absolute map directory>.\n\n## Question\n\n<the decision or investigation this ticket resolves>",
  "acceptance_criteria": "<what the decision must settle>. The resolution is recorded in tickets/<slug>.md and gisted on the map.",
  "dependencies": ["<name of a ticket blocking this one>"]
}
```

The type is one of `research`, `prototype`, `grilling`, `task` (see [Ticket Types](#ticket-types)).

A ticket's state lives in its ticket file, never in `tickets.json`, whose schema rejects unknown fields:

```markdown
Status: <claimed|closed|out-of-scope>

## Resolution

<the answer, what was done, and links to assets under ../assets/>
```

A session **claims** a ticket by writing its ticket file with `Status: claimed`, **first**, before any work, so concurrent sessions skip it. That file _is_ the claim: an open ticket with no ticket file is unclaimed.

Blocking is the task's `dependencies`: the names of the tickets that must close before this one can be worked. A ticket is **unblocked** when every ticket it depends on has `Status: closed` or `Status: out-of-scope`; the **frontier** is the tickets with no ticket file whose dependencies are all closed, the edge of the known, in `tickets.json` order.

The answer isn't part of the task; it's recorded on resolution (see [Work through the map](#work-through-the-map)). Assets created while resolving a ticket go under `assets/` and are linked from the ticket file, not pasted in.

## Ticket Types

Every ticket is either **HITL** (human in the loop, worked _with_ a human who speaks for themselves) or **AFK**, driven by the agent alone. A HITL ticket only resolves through that live exchange; the agent never stands in for the human's side of it (a grilling agent that answers its own questions has broken this). A subagent working a HITL ticket asks the human directly.

- **Research** (AFK): Reading documentation, third-party APIs, or local resources like knowledge bases to surface a fact a decision waits on. Resolved by a subagent that writes its findings under `assets/` and links them from the ticket file. Use when knowledge outside the current working directory is required.
- **Prototype** (HITL): Raise the fidelity of the discussion by making a cheap, rough, concrete artifact to react to (an outline, a rough take, a stub, or UI/logic code) under `assets/`. Links the prototype from the ticket file. Use when "how should it look" or "how should it behave" is the key question. When several variants are built, the human picks one; the agent never chooses and closes.
- **Grilling** (HITL): Conversation. The default case. Always use the `$grilling` and `$domain-modeling` skills.
- **Task** (HITL or AFK): Manual work that must happen before a _decision_ can be made: nothing to decide, prototype, or research, but the discussion is blocked until it's done. Signing up for a service so its API can be judged, provisioning access, moving data so its shape can be seen. This is the one type that _does_ rather than decides, and it earns its place by unblocking a decision, not by delivering the destination. The agent drives it alone where it can (AFK); otherwise it hands the human a precise checklist (HITL). Resolved when the work is done; the answer records what was done and any resulting facts (credentials location, new URLs, row counts) later tickets depend on.

## Fog of war

Keep the map deliberately incomplete. Record only tickets whose questions you can state precisely. The **fog of war** means decisions and investigations that you expect but cannot yet define because they depend on open questions. After you resolve a ticket, create new tickets for any questions you can now define. Repeat until no decisions remain before execution.

Use the map's **Not yet specified** section for suspected questions and areas to revisit. Everything in this section is in scope but not yet precise enough for a ticket. Record as much detail as you can to help collaborators understand the expected work.

**Fog or ticket?** Determine whether you can state the question precisely now, not whether you can answer it now.

- **Ticket when** the question is precise, even if a dependency prevents work on it now.
- **Not yet specified when** you cannot yet state the question precisely. Do not divide these areas into ticket-sized pieces in advance. An area may later produce several tickets or none.

**Not yet specified** excludes what's already decided (Decisions so far), what's already a live ticket, and what's out of scope (the next section).

## Out of scope

The destination determines the scope. Record work that you deliberately exclude from this effort in the map's **Out of scope** section. Do not put it in **Not yet specified**. Use scope, not the precision of a question, to select the section.

Out-of-scope work does not become a ticket as decisions are resolved. Return to it only if the destination changes. Treat that return as a new effort, not a resumption.

If an existing ticket is outside the scope, **close it** with `Status: out-of-scope` in its ticket file. This applies whether the original scope was incorrect or a resolution exposed the scope error. The ticket must no longer appear among tickets available for work. Add one line to **Out of scope** with a summary, the reason for exclusion, and a link to the ticket file. Do not add it to **Decisions so far**, which records resolved decisions, not scope exclusions.

## Invocation

Two modes. Either way, **never resolve more than one ticket per session**, with the exception of research tickets.

### Chart the map

User invokes with a loose idea.

1. **Name the destination.** Use the `$grilling` and `$domain-modeling` skills to pin down what this map is finding its way to: the spec, decision, or change. The destination fixes the scope, so it's settled first.
2. **Map the frontier.** Grill again, **breadth-first** this time: fan out across the whole space rather than deep on any one thread, surfacing the open decisions and the first steps takeable now. **If this surfaces no fog** (the way to the destination is already clear, the whole journey small enough for one session), you don't need a map. Stop and ask the user how they'd like to proceed.
3. **Create the map** (`map.md`): Destination and Notes filled in, Decisions-so-far empty, the fog sketched into **Not yet specified**.
4. **Create the tickets you can specify now** in `tickets.json`, with each ticket's `dependencies` naming the tickets that block it. Wiring sorts them into the frontier and the blocked; everything you can't yet specify stays in the fog: the **Not yet specified** section.
5. **Fire the research subagents.** Call `set_agent_tasks` with inline `tasks` holding the unblocked `research` tickets, copied from `tickets.json` without their `dependencies`, so they resolve in parallel. Each subagent claims, resolves, and closes its ticket, writing findings under `assets/`; add each one's line to Decisions so far when it returns.
6. Stop: charting is one session's work; it hand-resolves nothing.

### Work through the map

User invokes with a map (its directory, or its name under `$AGENT_SCRATCH_DIR/wayfinder/`). A ticket is **optional**: without one, you pick the next decision, not the user.

1. Load the **map**: `map.md` and the names, descriptions, and dependencies in `tickets.json`, not every ticket file.
2. Choose the ticket. If the user named one, use it. Otherwise take the first frontier ticket. **Claim it**: write its ticket file with `Status: claimed` before any work.
3. Resolve it. **Zoom as needed**: read the full task or ticket file of any related or closed ticket on demand; use whichever skills the `## Notes` block names. If in doubt, use the `$grilling` and `$domain-modeling` skills.
4. Record the resolution: write the answer under **Resolution** in its ticket file, set `Status: closed`, and **append a context pointer** to the map's Decisions-so-far.
5. Add newly-surfaced tickets to `tickets.json`; graduate any fog the answer has made specifiable, clearing each graduated patch from **Not yet specified** so it lives only as its new ticket. If the answer reveals that a ticket (this one or another) sits beyond the destination, **rule it out of scope** rather than resolving it on the route. If the decision invalidates other parts of the map, update those tickets, or delete them from `tickets.json` along with every dependency naming them.

The user may run unblocked tickets in parallel, so expect other sessions to be editing the map concurrently: re-read a file right before editing it.

## Attribution

Adapted from the `wayfinder` skill in [mattpocock/skills](https://github.com/mattpocock/skills/blob/main/skills/engineering/wayfinder/SKILL.md) by Matt Pocock. The issue tracker is replaced by agent task files in the agent scratch directory, and the `prototype` and `research` skills it calls are folded into the ticket types. MIT licensed, see `LICENSE.txt` in this directory.
