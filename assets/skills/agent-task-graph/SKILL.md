---
name: agent-task-graph
description: Use when handling very large tasks to author AgentTask graphs, splitting work so each leaf is executable and verifiable by a single medium agent while preserving useful independent parallel work.
---

# AgentTask Graph

Size the graph so each leaf is within a single medium agent's capacity to execute and verify, not merely understand or decompose. Author the graph upfront so the plan is visible, but keep it provisional: agents may refine it as work progresses.

## Choose the shape

1. Assess whether one medium agent can execute and verify the current task as a leaf, with the needed context, prerequisites, and observable acceptance criteria; also assess whether the work offers a useful independent parallel split.
2. When the task is within that execution-and-verification capacity, no further composite expansion is needed for capacity reasons; still consider useful independent parallel splits.
3. If it exceeds that capacity **or** the work can usefully run in parallel, split it into **2–7 tasks at a similar altitude**: peer outcomes or responsibility areas, not a mixture of broad programs and implementation steps.
4. Apply both tests recursively to each resulting task, making composites with 2–7 peer children where either calls for a split. Stop when each leaf is executable and verifiable by one medium agent and no useful independent parallel split remains; do not fragment atomic tasks merely to create more children.

When authoring a breakdown, reduce duplicated implementation, setup, and validation by assigning shared work once, in preparation, conclusion, or a dedicated task. Shared preparation may include research, a shared scratchpad, or queues when needed; conclusion may include integrating or merging results and cleaning up task-created worktrees. Shared queues must be created and owned by the consumers’ direct parent; a preparation task may supply their inputs. Make preparation a prerequisite of the children that need it, and conclusion depend on the relevant children. Omit setup that the work does not need.

Look for opportunities to save time by running expensive common steps once in a dedicated task rather than repeating them in each task.

Prefer independent tasks that can run in parallel, using separate git worktrees for repository changes. Preserve genuine dependencies rather than forcing parallelism. For repository changes, hand off commit/worktree references and assign who incorporates them. Incorporate prerequisite commits into the dependent task’s worktree before work requiring them begins.

## Encode and hand off

Use `set_agent_tasks` to declare the graph you own. A leaf has a direct instruction string as its `payload`. A composite has its child task definitions as a `payload` array.

A composite's array is passed to its **retained child agent as prompt content**; nested arrays do **not** automatically register graphs. That child must explicitly call `set_agent_tasks` to declare and manage its own graph, supervise its children, validate their combined result, and return the composite's outcome. Revisions and retries continue in the retained child session.

Give every task a bounded scope, needed context, prerequisites, and observable acceptance criteria; a leaf is ready when a medium agent can use these to execute and verify it, without needing every implementation decision prescribed. Keep siblings at comparable abstraction levels with clear ownership boundaries. Dependencies name prerequisite tasks in the same owner's graph; add them for required results, not merely preferred ordering. Make each handoff identify the result its dependents need, and make composite acceptance cover the integrated outcome rather than only child completion.
