# Predefined configuration resources

The following resources were retained and subsequently adapted from the .NET source at commit
`5f27a8a92625ee31d8bec094e5c3e09550193781`:

- `predefined_config.system_prompts.yaml`
- `predefined_config.agent_tasks.yaml`
- `predefined_config.profiles.yaml`
- `predefined_config.runtime.yaml`

The active native template resource is
`predefined_config.system_prompts.minijinja.yaml`. It uses MiniJinja for the ten
explicitly migrated descriptors and two consolidated message templates.
`predefined_config.agent_tasks.yaml` adds one consolidated MiniJinja template;
the remaining descriptors use the scalar engine. There is no general
Scriban-to-Jinja translator or automatic rewriting of user configuration.

## Consolidated message templates

Three complete-output templates replace thirteen fragment/variant keys, reducing
the active catalog from 107 to 97 entries. Default message wording is unchanged.

| Template | Consumer | Arguments |
| --- | --- | --- |
| `agent-task.assignment-context` | `crates/parrot-runtime/src/task_scheduler/runner.rs` | Task name, description, acceptance criteria, `dependencies` (name, description, bounded result), and `siblings` (name, bounded description). Both lists may be empty. |
| `compact-context-tool.result` | `crates/parrot-tools/src/session/tools.rs` | `outcome` (`compacted`, `no-op`, or `unavailable`), boolean `has_context_limit_override`, usage, estimated tokens, context limit, notification interval, trigger percentage, and trigger tokens. Missing usage/trigger-token estimates are the string `unavailable`. |
| `agent-session.nonempty-queues-reminder` | `crates/parrot-runtime/src/active_work/blockers.rs` | `queues`, a name-sorted list of objects with `name` and numeric `size`. Only nonempty owned queues are supplied; no reminder is emitted for an empty list. |

Custom overrides must target these descriptors and their structured arguments.
The former `agent-task.header`, `.dependency-header`, `.dependency-item`,
`.sibling-scope`, and `.sibling-item` keys, the six
`compact-context-tool.compacted`/`.no-op`/`.unavailable` keys (including their
`-with-limit` variants), and `agent-session.nonempty-queue-item` are no longer
used. The retained `agent-session.nonempty-queues-reminder` key now receives a
list rather than preformatted text. Old overrides are not automatically migrated
or supported through aliases.

## Agent questions and asks

`question` retains its structured user-question schema and user-away behavior,
but is advertised only to root agents. All agents use `ask` and `reply` for
plain-text inter-agent requests. `ask` accepts any valid relative or absolute
agent path in the same user session, independently of `agent_send` security
restrictions, and blocks until the recipient replies. `reply` settles the exact
request immediately; it does not wait for the recipient's turn to finish.
Pending incoming asks prevent recipient completion, and cycles fail explicitly.
The former `answer` tool is removed without an alias.

Both system-prompt resources expose these configurable descriptors:

| Template | Arguments |
| --- | --- |
| `agent-session.agent-ask` | `asker_name`, `request_id`, `message` |
| `agent-session.pending-ask-reminder` | `requests`, preformatted pending-request text including request IDs |
| `tool-result.ask-reply` | `message`, the returned plain-text reply |
| `tool-result.ask-replied` | `request_id`, the settled request |

The former `agent-session.child-question`,
`agent-session.pending-child-question-reminder`, and `tool-result.question-replied`
keys are removed. Custom overrides must migrate to the new descriptors; old
keys are not supported through aliases.

These are first-party application resources from the pinned source. No per-file
license header or root source-repository LICENSE was found for these resources;
no license grant is inferred from third-party skill licenses. See
[third-party notices](../../THIRD_PARTY_NOTICES.md) for retained dependency notices.
