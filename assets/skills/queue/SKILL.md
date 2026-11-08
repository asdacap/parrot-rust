---
name: queue
description: Coordinate shared work queues between agents using files, concurrent producers and consumers, and progress reports of waiting items. Use for many independent work items or producer/consumer pipelines.
---

# Queue

Use queues for many work items or multiple producers sharing consumers. Give one item type and one consumer assignment to each queue. For one item per child, spawn children directly. For dependencies between tasks, use AgentTasks.

## Prerequisites and location

The helper needs Python 3 on Linux or macOS and ordinary authorized command execution. Report an unavailable prerequisite or denied command; do not install software or bypass the security profile.

Resolve `scripts/queue.py` relative to this skill's directory and invoke it with `python3 -B`. Create queue files under the owning agent's `$AGENT_SCRATCH_DIR/queues/<name>.json`. Pass their **absolute paths** to workers. A child's `$AGENT_SCRATCH_DIR` points to the child, not its parent. Use only explicitly supplied queue paths; do not scan other agents' directories.

Use the helper for every queue operation, including reads. It locks a stable sidecar file and atomically replaces the state file. Leave the `.lock` file in place. Ordinary file permissions and command security remain authoritative.

## Operations

In these examples, `HELPER` is the absolute helper path and `QUEUE` is the owner's absolute queue file path. Quote paths when invoking commands.

```sh
python3 -B "$HELPER" create "$QUEUE" --description 'Review source files'
python3 -B "$HELPER" push "$QUEUE" --items '["src/a.rs","src/b.rs"]'
python3 -B "$HELPER" push "$QUEUE" --source-file "$ITEM_FILE" --close
python3 -B "$HELPER" take "$QUEUE" --count 5 --wait-ms 30000
python3 -B "$HELPER" info "$QUEUE"
python3 -B "$HELPER" close "$QUEUE"
python3 -B "$HELPER" summary "$QUEUE" "$OTHER_QUEUE"
```

`push --items` accepts a JSON array of strings, including an empty array. A source file contributes one item per nonblank UTF-8 line. `--close` appends its items and closes in one operation. For multiple producers, the owner or designated final closer waits for **all** producers before closing; an individual producer must not close while another can still append.

`take` removes a FIFO batch, with a default count of one. It waits only when the queue is open and empty. The default wait is zero; `--wait-ms` accepts 0 through 300000. Returned batches need not fill the requested count. Every result reports `closed`, `remaining`, `enqueued_total`, `taken`, and `drained`; `take` also returns the removed `items`.

Consumers repeat until `drained` is true. An empty open queue is temporary, not completion. A closed queue can still contain waiting items. Queue files survive process and session restarts; read them with `info` to resume coordination.

Taking an item means it is no longer waiting, even while its worker is running. There is no acknowledgement, automatic recovery of taken items, or exactly-once work guarantee. If a take is interrupted after committing but before its output arrives, inspect state and worker outcomes rather than blindly retrying. Keep work idempotent when external effects require it.

## Report remaining items with progress

The owner alone reads `summary` for its explicit list of queues and reports their aggregate through the existing `progress` tool:

- Set `steps` to `taken` and `total_steps` to `enqueued_total`.
- Set `current_step` to a label with `remaining`, for example `queue review: 3 waiting`.
- Ten admitted items with seven taken means `steps: 7`, `total_steps: 10`, and `3 waiting`, even if those seven are still running.
- Refresh after seeding, after the owner's changes, during normal coordination waits, and before closing the report. This is a snapshot, not an automatic file subscription. Workers need not send a message per item.
- Aggregate multiple queues into the owner's one report. This report replaces that agent's prior progress report. Summary reads each queue under its lock; simultaneous updates to different queues are not one transaction.
- If the total is zero, omit the progress update. Do not send `0/0` or invent a step. Keep coordinating an open empty queue. A never-populated drained queue needs no report.
- Once a nonzero report exists, keep it active through open-empty periods. Producers may add items and change the total, so the reported percentage can decrease.

When every selected queue is closed and empty, publish final counts and call `progress` with `close: true`. Reaching the total does not close progress. The counts measure work distribution, not successful completion; still join workers and check their outcomes before declaring their work complete. Closing queue progress does not clear child, task, process, or reminder checks.

On intentional stop, report the remaining waiting items and explicitly close progress. Leave queue files intact. Closing the report below its total does not assert success or discard work.

Old built-in queues under the agent root's `queues/` directory are not skill queues. Leave them untouched; this skill does not migrate or reattach them.
