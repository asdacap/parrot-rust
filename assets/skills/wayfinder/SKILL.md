---
name: wayfinder
description: Plan a huge chunk of work (more than one agent session can hold) as a shared map of decision tickets kept as agent tasks in the agent scratch directory, and resolve them one at a time until the way to the destination is clear.
---

A loose idea has arrived, too big for one agent session, and wrapped in fog: the way from here to the **destination** isn't visible yet. Wayfinding is about finding that way, not charging at the destination. This skill charts the way as a **shared map** in the agent scratch directory, then works its **decision tickets** (questions whose resolution is a decision, not slices of a build to execute) one at a time until the route is clear.

The destination varies per effort, and naming it is the first act of charting: it shapes every ticket. It might be a spec to hand off and iterate on, a decision to lock before planning starts, or a change made in place like a data-structure migration. The map is domain-agnostic: engineering work, course content, whatever fits the shape.

## Plan, don't do

Wayfinder is **planning** by default: each ticket resolves a decision, and the map is done when the way is clear, with nothing left to decide before someone goes and does the thing. The pull to just do the work is usually the signal you've reached the edge of the map and it's time to hand off. An effort can override this in its **Notes**, carrying execution into the map itself, but absent that, produce decisions, not deliverables.

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

The map is _deliberately_ incomplete: don't chart what you can't yet see. Beyond the live tickets lies the **fog of war**: the dim view of decisions and investigations you can tell are coming but can't yet pin down, because they hang on questions still open. Resolving a ticket clears the fog ahead of it, graduating whatever's now specifiable into fresh tickets, one at a time, until the way to the destination is clear and no tickets remain.

The map's **Not yet specified** section is where that dim view is written down: the suspected question, the area to revisit later. It's the undiscovered frontier _toward_ the destination: everything here is in scope, just not sharp enough to ticket. Write as loosely or as fully as the view allows; it doubles as a signpost for collaborators reading where the effort is headed.

**Fog or ticket?** The test is whether you can state the question precisely now, _not_ whether you can answer it now.

- **Ticket when** the question is already sharp, even if it's blocked and you can't act on it yet.
- **Not yet specified when** you can't yet phrase it that sharply. Don't pre-slice the fog into ticket-sized pieces: it's coarser than a ticket, and one patch may graduate into several tickets, or none, once the frontier reaches it.

**Not yet specified** excludes what's already decided (Decisions so far), what's already a live ticket, and what's out of scope (the next section).

## Out of scope

Fog only ever gathers _toward_ the destination. The destination fixes the scope, so work beyond it is **out of scope**: it isn't fog, and it doesn't belong in **Not yet specified**. It gets its own **Out of scope** section on the map: work you've consciously ruled out of _this_ effort. Scope, not sharpness, lands it here.

Out-of-scope work never graduates (the frontier stops at the destination), so it returns only if the destination is redrawn, and then as a fresh effort, not a resumption.

Ruling something out of scope is a scoping act, not a step on the route. When a ticket that already exists turns out to sit past the destination (mis-scoped in while charting, or exposed by a resolution), **close it** with `Status: out-of-scope` in its ticket file (so it is unambiguously off the frontier) and leave one line in the **Out of scope** section: the gist plus why it's out of scope, linking the ticket file. It stays out of **Decisions so far**, which records the route actually walked; a scope boundary isn't a step on it.

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
