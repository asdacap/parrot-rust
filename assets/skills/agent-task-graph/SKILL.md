---
name: agent-task-graph
description: Use when handling very large tasks to author AgentTask graphs, splitting work so each leaf is executable and verifiable by a single medium agent while preserving useful independent parallel work.
---

# AgentTask Graph

Size the graph so each leaf is within a single medium agent's capacity to execute and verify, not merely understand or decompose. Author the graph upfront so the plan is visible, but keep it provisional: agents may refine it as work progresses.

## Choose the shape

There is no fixed limit on the total number of leaf tasks across the whole graph. Plans and tasks vary widely in size, so overall graph size may vary widely. The purpose is to break large plans into identifiable, finite-sized leaves, each scoped so a medium agent can confidently execute and verify it. Large graphs are intentional; when leaf size is uncertain, err toward splitting, since oversized tasks are more likely to miss requirements and underestimate the work.

Planning/decomposition capacity is distinct from execution capacity: a task one agent could further decompose may still be a composite when it contains multiple substantial deliverables. A composite owner is responsible for decomposition and integration; its children may run sequentially. A leaf is one bounded, cohesive work package that its assigned agent can implement and verify without another substantial breakdown.

1. Before accepting each candidate as an executable leaf, the planner—whoever is planning or decomposing the current task—must spawn a dedicated read-only decision subagent. Give it the candidate's full payload, acceptance criteria, relevant context, and dependencies. Use one decision assignment per candidate, not a batch review or the planner's self-review.
2. Have that subagent decide whether one medium agent can execute and verify the candidate with those inputs, and whether the work offers a useful independent parallel split. Require a leaf-or-split verdict with rationale and, for a split, a proposed breakdown into **2–7 tasks at a similar altitude**: peer outcomes or responsibility areas, not a mixture of broad programs and implementation steps.
3. The planner must incorporate a split verdict's breakdown rather than accepting that candidate as a leaf: split when it exceeds that execution-and-verification capacity **or** can usefully run in parallel. The decision subagent assesses only its candidate; it does not implement work or recursively spawn decision reviewers.
4. The planner must repeat this process at every recursion depth, using fresh dedicated decision assignments for resulting candidate leaves and any newly introduced or materially expanded leaves. Stop only when every leaf has received a leaf verdict, is executable and verifiable by one medium agent, and has no useful independent parallel split remaining; do not fragment atomic tasks merely to create more children.

Keep verification proportional to scope: large or broad verification belongs in its own task; leaf verification should generally be focused, such as appropriate unit-level checks. Include other useful focused checks when they fit the leaf.

Use these dedicated decisions as the execution-readiness review, separately from checking schema validity, dependency correctness, and scope coverage. Have each decision subagent inspect its candidate's full payload and acceptance criteria, not just its title: it should name a cohesive responsibility, have required interfaces and inputs available or assigned through dependencies, and specify focused validation that demonstrates completion. Multiple substantial deliverables that can be implemented and tested separately suggest a composite, even when its children must run sequentially. Do not split merely by file count, lines of code, or arbitrary duration; keep tightly coupled implementation and tests together. Integration should assemble delivered components, not conceal missing subsystem implementation.

Review the broadest candidates first, then the remaining candidates. Apply required splits while preserving peer grouping and the atomic-task guardrail. If decomposition is intentionally deferred, represent that work as a planning outline with composite boundaries, not as executable leaves.

When authoring a breakdown, reduce duplicated implementation, setup, and validation by assigning shared work once, in preparation, conclusion, or a dedicated task. Shared preparation may include research, a shared scratchpad, or queues when needed; conclusion may include integrating or merging results and cleaning up task-created worktrees. Shared queues must be created and owned by the consumers’ direct parent; a preparation task may supply their inputs. Make preparation a prerequisite of the children that need it, and conclusion depend on the relevant children. Omit setup that the work does not need.

Look for opportunities to save time by running expensive common steps once in a dedicated task rather than repeating them in each task.

Prefer independent tasks that can run in parallel, using separate git worktrees for repository changes. Preserve genuine dependencies rather than forcing parallelism. For repository changes, hand off commit/worktree references and assign who incorporates them. Incorporate prerequisite commits into the dependent task’s worktree before work requiring them begins.

## Encode and hand off

Use `set_agent_tasks` to declare the graph you own. A leaf has a direct instruction string as its `payload`. A composite has its child task definitions as a `payload` array.

A composite's array is passed to its **retained child agent as prompt content**; nested arrays do **not** automatically register graphs. That child must explicitly call `set_agent_tasks` to declare and manage its own graph, supervise its children, validate their combined result, and return the composite's outcome. Revisions and retries continue in the retained child session.

Give every task a bounded scope, needed context, prerequisites, and observable acceptance criteria; a leaf is ready when a medium agent can use these to execute and verify it, without needing every implementation decision prescribed. Keep siblings at comparable abstraction levels with clear ownership boundaries. Dependencies name prerequisite tasks in the same owner's graph; add them for required results, not merely preferred ordering. Tasks receive dependency context by default, so they can recognize early when prerequisite work has already satisfied their task and skip that satisfied work; this is not permission to skip unmet work. Make each handoff identify the result its dependents need, and make composite acceptance cover the integrated outcome rather than only child completion.

Focus on the requested output: it is the evidence. Evidence or provenance reports are usually wasted effort; produce them only when explicitly requested. Keep temporary process reports out of the repository, using scratch only when actually needed.
