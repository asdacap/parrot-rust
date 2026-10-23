---
name: agent-task-graph
description: Use when handling very large tasks to author AgentTask graphs.
---

# AgentTask Graph

Size the graph so each leaf is within a single medium agent's capacity to execute and verify, not merely understand or decompose. Author the graph upfront so the plan is visible, but keep it provisional: agents may refine it as work progresses.

## Choose the shape

There is no fixed limit on the total number of leaf tasks across the whole graph. Plans and tasks vary widely in size, so overall graph size may vary widely. The purpose is to break large plans into identifiable, finite-sized leaves, each scoped so a medium agent can confidently execute and verify it. Large graphs are intentional; when leaf size is uncertain, err toward splitting, since oversized tasks are more likely to miss requirements and underestimate the work.

Planning/decomposition capacity is distinct from execution capacity: a task one agent could further decompose may still be a composite when it contains multiple substantial deliverables. A composite owner is responsible for decomposition and integration; its children may run sequentially. A leaf is one bounded, cohesive work package that its assigned agent can implement and verify without another substantial breakdown.

Follow every step below for each candidate; planner-led evaluation, reviewer reuse, and batching do not permit skipping steps.

1. Before accepting each candidate as an executable leaf, individually evaluate its full payload, acceptance criteria, relevant context, and dependencies. The planner may perform this evaluation or assign it to a reviewer; reviewers may be reused or assigned batches of candidates.
2. For every candidate, make a distinct leaf-or-split decision with rationale: determine whether one medium agent can execute and verify it with those inputs, and whether the work offers a useful independent parallel split. Record the sizing rationale for each candidate in a shared scratch file outside the repository, with each entry identified by task name or path and based on its full payload, acceptance criteria, context, and dependencies. For a leaf, explain concretely why one medium agent can execute and verify its bounded, cohesive work without substantial further decomposition and why no useful independent split remains. For a split or composite, explain why a child breakdown is needed and appropriate. If split, normally propose a breakdown into **2–7 tasks at a similar altitude**: peer outcomes or responsibility areas, not a mixture of broad programs and implementation steps. A batch-level verdict cannot substitute for an individual verdict for each candidate.
   - Each leaf rationale must identify its hardest implementation obligation, its focused completion test, and the strongest plausible split. Explain why that split would duplicate tightly coupled work or produce no useful independent deliverable; otherwise apply it. Merely sharing a subsystem, interface, or fixture family is not sufficient reason to keep work together.
3. Incorporate each split verdict's breakdown rather than accepting that candidate as a leaf: split when it exceeds that execution-and-verification capacity **or** can usefully run in parallel. A delegated decision reviewer assesses only its assigned candidates; it does not implement work or recursively spawn decision reviewers.
   - For multi-subsystem programs, use composite tasks for major responsibility areas rather than flattening implementation leaves just to make their dependencies easier to inspect. When splitting a candidate, retain it as a composite with a child-task array payload, recursively, unless the grouping has no meaningful shared responsibility, preparation, integration, or acceptance outcome. Record the rationale for any flattening during the readiness review.
   - The composite owner handles shared preparation and interfaces, declares and supervises its children, integrates and verifies the composite acceptance outcome, and returns a coherent result. Keep siblings at comparable responsibility levels. Small, cohesive tasks may remain flat; do not create composites solely for numerical branching or as meaningless single-child wrappers.
4. Repeat this process at every recursion depth, individually evaluating resulting candidates and any newly introduced or materially expanded candidates; add or update their named sizing-rationale entries when candidates materially change. Stop only when every leaf has received an individual leaf verdict, is executable and verifiable by one medium agent, and has no useful independent parallel split remaining; do not fragment atomic tasks merely to create more children.
5. Before accepting or materially revising a graph, apply this structural gate at every owner’s level, including nested composite payloads, separately from leaf readiness:
   - Each major responsibility area has an explicit owner.
   - Each split candidate remains a retained owner, or its flattening has the readiness-review rationale described above.
   - Composite acceptance requires an integrated outcome, not merely completed children.
   - Cross-composite dependencies describe deliverables. If an early interface is needed, extract a shared prerequisite or refine the boundaries rather than flattening the graph.
   - Reject flattening at any depth that bypasses meaningful composite ownership, even if each resulting leaf is small. Apply the grouping exceptions and rationale requirement above.
   - When authoring or materially revising any owner’s graph, including the root, review its direct-child count, including preparation and conclusion tasks. Aim for 2–7 where meaningful grouping permits; record a rationale for higher fanout rather than creating artificial composites. Do not split cohesive work merely to reach the range.

Keep verification proportional to scope: large or broad verification belongs in its own task; leaf verification should generally be focused, such as appropriate unit-level checks. Include other useful focused checks when they fit the leaf.

Use these individual decisions as the execution-readiness review, separately from checking schema validity, dependency correctness, and scope coverage. The evaluator must inspect each candidate's full payload and acceptance criteria, not just its title: it should name a cohesive responsibility, have required interfaces and inputs available or assigned through dependencies, and specify focused validation that demonstrates completion. Multiple substantial deliverables that can be implemented and tested separately suggest a composite, even when its children must run sequentially. Do not split merely by file count, lines of code, or arbitrary duration; keep tightly coupled implementation and tests together. Integration should assemble delivered components, not conceal missing subsystem implementation.

Review the broadest candidates first, then the remaining candidates. Apply required splits while preserving peer grouping and the atomic-task guardrail. If decomposition is intentionally deferred, represent that work as a planning outline with composite boundaries, not as executable leaves.

When authoring a breakdown, reduce duplicated implementation, setup, and validation by assigning shared work once, in preparation, conclusion, or a dedicated task. Shared preparation may include research, a shared scratchpad, or queues when needed; conclusion may include integrating or merging results and cleaning up task-created worktrees. Shared queues must be created and owned by the consumers’ direct parent; a preparation task may supply their inputs. Make preparation a prerequisite of the children that need it, and conclusion depend on the relevant children. Omit setup that the work does not need.

Look for opportunities to save time by running expensive common steps once in a dedicated task rather than repeating them in each task.

Prefer independent tasks that can run in parallel, using separate git worktrees for repository changes. Preserve genuine dependencies rather than forcing parallelism. For repository changes, hand off commit/worktree references and assign who incorporates them. Incorporate prerequisite commits into the dependent task’s worktree before work requiring them begins.

## Encode and hand off

Use `set_agent_tasks` to declare the graph you own. A leaf has a direct instruction string as its `payload`. A composite has its child task definitions as a `payload` array.

A composite's array is passed to its **retained child agent as prompt content**; nested arrays do **not** automatically register graphs. That child must explicitly call `set_agent_tasks` to declare and manage its own graph, supervise its children, validate their combined result, and return the composite's outcome. Revisions and retries continue in the retained child session.

Give every task a bounded scope, needed context, prerequisites, and observable acceptance criteria; a leaf is ready when a medium agent can use these to execute and verify it, without needing every implementation decision prescribed. Keep siblings at comparable abstraction levels with clear ownership boundaries. Dependencies name prerequisite tasks in the same owner's graph; add them for required results, not merely preferred ordering. Tasks receive dependency context by default, so they can recognize early when prerequisite work has already satisfied their task and skip that satisfied work; this is not permission to skip unmet work. Make each handoff identify the result its dependents need, and make composite acceptance cover the integrated outcome rather than only child completion.

Focus on the requested output: it is the evidence. Evidence or provenance reports are usually wasted effort; produce them only when explicitly requested, except for the required per-task sizing rationale recorded in scratch. Keep temporary process reports out of the repository, using scratch only when actually needed.
