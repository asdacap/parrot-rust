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
The historical `predefined_config.system_prompts.yaml` remains a reference
resource, not the active production resource.

## Built-in system prompts

The seven static `system_prompts` bodies live in `assets/config/system-prompts/`:

| Provider key | Markdown file |
| --- | --- |
| `runtime:system-context:01-base` | `base.md` |
| `runtime:system-context:02-delegation` | `delegation.md` |
| `runtime:system-context:02a-agent-tasks` | `agent-tasks.md` |
| `runtime:system-context:03-subagent-pattern` | `subagent-patterns.md` |
| `runtime:system-context:04-behavior-guideline` | `behavior-guidelines.md` |
| `runtime:system-context:04a-markdown-reading` | `markdown-reading.md` |
| `runtime:system-context:05-git-ethics` | `git-ethics.md` |

Predefined configuration selects these files with reserved string values such as
`@builtin:system-prompts/base.md`. The files are embedded at compile time. A
value beginning with the exact, case-sensitive prefix `@builtin:` must be a
complete known reference; an empty or unknown reference is a configuration
error. The suffix identifies a bundled asset, not an arbitrary filesystem path.
Other nonempty values are literal prompt text, including text with the marker
elsewhere in the value. Additional namespaced provider keys remain supported,
and any valid namespaced key can use a known built-in reference.

Override an inherited key with inline text in `config.yaml`, for example:

```yaml
system_prompts:
  runtime:system-context:01-base: |-
    Your replacement instructions.
```

If `system_prompts` already exists, add or replace the key in that mapping.
Remove the override to restore the built-in default. The generated
`predefined_config.yaml` retains references and is regenerated; do not edit it
for customization. Editing installed Markdown does not change the built binary,
and source Markdown changes require a rebuild. Neither is hot-reloaded.
Dynamic `prompt_templates`, model-specific `system_prompt` values, and model
alias augmentation remain in configuration.

## Built-in profile prompts

The ten agent-profile prompt bodies live in `assets/config/profile-prompts/`:

- [`general.md`](profile-prompts/general.md)
- [`coordinator.md`](profile-prompts/coordinator.md)
- [`build.md`](profile-prompts/build.md)
- [`plan.md`](profile-prompts/plan.md)
- [`query.md`](profile-prompts/query.md)
- [`explorer.md`](profile-prompts/explorer.md)
- [`review.md`](profile-prompts/review.md)
- [`worker.md`](profile-prompts/worker.md)
- [`thinker.md`](profile-prompts/thinker.md)
- [`agent-task-payload.md`](profile-prompts/agent-task-payload.md)

Predefined configuration selects each file with a value such as
`@builtin:profile-prompts/build.md` in `profiles.build.prompt`. These Markdown
files are embedded at compile time. References resolve after user configuration
is merged. A value beginning with the exact, case-sensitive `@builtin:` prefix
must select a known built-in asset; an empty or unknown reference reports an
error at `profiles.<id>.prompt`. Other nonempty text is literal, and resolved
Markdown is not recursively interpreted as another reference. References do not
load arbitrary filesystem paths or hot-reload installed files.

To replace a profile prompt, put inline text in `config.yaml`:

```yaml
profiles:
  build:
    prompt: |-
      Your replacement instructions.
```

Remove `profiles.build.prompt` to restore the built-in default. Profile usage,
allowed tools, flags, and limits are unchanged unless explicitly overridden.
The generated `predefined_config.yaml` retains the reference values and is
regenerated; source Markdown changes require rebuilding the binary.

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
