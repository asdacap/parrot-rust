# Configuration, legacy state and rollback

**Existing databases are not migrated.** This build supports only its current
SQLite format. An incompatible session fails to resume instead of being
converted, reset, or deleted. Its database and session artifacts remain in place.
This SQLite policy is separate from the supported
[AgentTask snapshot migration](#agenttask-snapshot-migration), which can update
retained recovery files without changing the database format.

Use `parrot chat --new-session` to start a different local session without
resuming or replacing the old one. Normal session creation and ownership checks
still apply. `/clear` is available only after chat starts, so it cannot resolve a
startup refusal.

Filesystem-root fallback remains supported. It selects a directory, not a
compatible database format. See
[database formats and fresh startup](#database-formats-and-fresh-startup).

## Roots and manual configuration/credential copy

Only the application directory component changes, not workspace identities,
command names, user content, environment variable names or historical text:

| Kind | Rust root | Legacy .NET root |
| --- | --- | --- |
| Config | `${XDG_CONFIG_HOME:-$HOME/.config}/parrotrust` | `${XDG_CONFIG_HOME:-$HOME/.config}/parrotdotnet` |
| State | `${XDG_STATE_HOME:-$HOME/.local/state}/parrotrust` | `${XDG_STATE_HOME:-$HOME/.local/state}/parrotdotnet` |
| Data | `${XDG_DATA_HOME:-$HOME/.local/share}/parrotrust` | `${XDG_DATA_HOME:-$HOME/.local/share}/parrotdotnet` |

Empty XDG variables use the HOME defaults. Both roots use the **same current
HOME/XDG resolution**; discovery does not search earlier homes. Do not symlink
one root to the other or globally replace the string `parrotdotnet` in stored
paths/text.

Configuration and credentials are **new-root only**, regardless of fallback:

- Manually copy/review `config.yaml` if desired; preserve unrelated keys, values
  and literal alias names, including `low_llm`. The execution-time ban on low-tier
  helpers is not a rename or deletion of the A002 configuration strings.
- Manually copy `credentials.json` and selected `credential_presets/<name>.json`
  only if you deliberately want those accounts available to Rust. Presets contain
  secrets just like the credential store; environment-based keys/provider headers
  are not transferred by copying a preset. Do not print or commit their contents.
- Keep directories owner-only and credential files owner-readable/writable only.
  Do not overwrite an existing new-root configuration/store/preset without
  reviewing and backing it up. No auth/provider/OAuth/S3 command is needed for a
  filesystem copy, and none is authorized or tested by this documentation task.
- Do not copy/edit generated `predefined_config.yaml` as the user override: Rust
  refreshes that readable default copy. Put intentional overrides in `config.yaml`.
  Explicit old Scriban templates must be manually adapted to MiniJinja; user
  templates are not automatically rewritten. Back up custom templates before
  making a manual change.

Optional **user-action** example, after stopping both applications and making
backups. These commands are per-file manual copies, not a state migration. Check
that the selected sources exist; answer **no** to an overwrite unless you have
reviewed the destination. Run the commands interactively, one at a time: declining
`cp -i` can return nonzero (and stop a shell using `set -e`); that is not permission
to force the overwrite. T81 checked this pattern with synthetic dummy files only,
never real credentials:

```sh
old_config="${XDG_CONFIG_HOME:-$HOME/.config}/parrotdotnet"
new_config="${XDG_CONFIG_HOME:-$HOME/.config}/parrotrust"
umask 077
mkdir -p "$new_config"
chmod 700 "$new_config"
cp -i "$old_config/config.yaml" "$new_config/config.yaml"
cp -i "$old_config/credentials.json" "$new_config/credentials.json"
chmod 600 "$new_config/config.yaml" "$new_config/credentials.json"
```

Copy each desired preset separately into the new root's owner-only
`credential_presets` directory; do not bulk-copy config/data/state trees. Missing
new config, credentials or presets **never** cause automatic legacy lookup.
Copying credentials does not authorize a network action, and remote attachment
does not move the CLI machine's credential store onto the server.

## System prompt overrides

Use inline text in `config.yaml` to replace an inherited system prompt. For
example, add the following key to an existing `system_prompts` mapping, or add the
mapping if it does not exist:

```yaml
system_prompts:
  runtime:system-context:01-base: |-
    Your replacement instructions.
```

Removing that key restores its built-in default. Other inherited prompts remain
unchanged, and additional namespaced provider keys can be added with nonempty
inline strings or block scalars.

The seven built-in static system prompts are Markdown files under
`assets/config/system-prompts/`, embedded into the binary at compile time. The
[resource reference](../../assets/config/README-resources.md#built-in-system-prompts)
lists their provider keys and filenames. Predefined configuration uses values
such as `@builtin:system-prompts/base.md` to select these bundled assets. A value
beginning with the exact, case-sensitive `@builtin:` prefix must be a complete
known reference; an empty or unknown reference is a configuration error. Any
other nonempty value remains literal text. References do not load arbitrary
filesystem paths.

The generated `predefined_config.yaml` retains the references and is regenerated;
put overrides in `config.yaml` rather than editing it. Editing installed Markdown
does not change the built binary, and source Markdown changes require a rebuild.
Neither is hot-reloaded. Dynamic `prompt_templates`, model-specific
`system_prompt` values, and model alias augmentation remain in configuration.

## Profile prompt overrides

Use inline text in `config.yaml` to replace an inherited agent-profile prompt:

```yaml
profiles:
  build:
    prompt: |-
      Your replacement instructions.
```

Add or replace `prompt` within an existing `profiles.build` mapping if present.
Removing that field restores the built-in default. Other profile fields, such as
usage, allowed tools, flags, and limits, remain unchanged unless overridden.

The ten built-in agent-profile prompts are Markdown files under
`assets/config/profile-prompts/`, embedded into the binary at compile time. The
[resource reference](../../assets/config/README-resources.md#built-in-profile-prompts)
links all ten files. Predefined configuration selects them using values such as
`@builtin:profile-prompts/build.md`. References resolve after user configuration
is merged. A value beginning with the exact, case-sensitive `@builtin:` prefix
must select a known built-in asset. An empty or unknown reference reports an
error at `profiles.<id>.prompt`. Other nonempty text remains literal, and
resolved Markdown is not recursively interpreted as another reference.

References do not load arbitrary filesystem paths. The generated
`predefined_config.yaml` retains the references and is regenerated, so put
overrides in `config.yaml`. Editing installed Markdown does not change the
binary; source Markdown changes require rebuilding and are not hot-reloaded.

## Filesystem-root fallback

The top-level YAML boolean **`legacy_path_read_fallback` defaults to `true`**.
It enables old-root discovery and eligible file fallback, not support for old
database formats. To disable this fallback, edit the top-level key in the new-root
`config.yaml`. Do not append a duplicate key. This is not a CLI switch:

```yaml
legacy_path_read_fallback: false
```

This disables future old-root session discovery and eligible file fallback. It
is **not** a filesystem deny rule for explicit, normally authorized access to an
old pathname. It does not undo earlier writes, relocate/close an already selected
session, or split its resources. Ownership safety still checks both current and
legacy roots even when discovery fallback is disabled.

## Selection, read precedence and no resource mixing

| Operation | Behavior |
| --- | --- |
| Fresh session | Always new state root. No automatic migration. |
| Existing session / duplicate ID | Select the new-root session directory if present; only if absent and fallback enabled consider the legacy directory. Never merge duplicates. |
| Selected legacy-root session | Keep its DB and resources in that root. Resume requires the current database format. Refuse an incompatible DB without converting it or trying the other root. |
| Selected session's missing internal file | Stay in that selected root; report the error or use source-defined recovery. Never substitute the other root's artifact/queue/DB. |
| Eligible general state/data read or historical absolute old-root reference | New corresponding file first, old file only when the primary is absent and fallback enabled. Preserve the stored historical text. |
| Permission error, malformed/corrupt primary or invalid session root | Report it; do not hide it by trying the other root. |
| Config, credentials, presets | New-root only; no legacy fallback. |

Once selected, a session keeps its DB, queues, artifacts, scratch, agent history
projections, logs, sockets and ownership/activation records under that selected
state root. Examples include `session.db`, `meta.json`, `session.log`,
`parrot.sock`, `queues`, `artifacts`, `scratch` and `root-agents`. Ordinary resume
does not relocate the working directory or rewrite historical text. Do not repair
an incomplete session by copying selected files from a same-ID session in the
other root. A session-root directory that is a symlink/file is rejected; general
read fallback must still authorize the actual resolved target, including symlinks.
Root selection/fallback itself grants **no access permission** and does not repair
the unresolved production filesystem/security capability.

## Database formats and fresh startup

New sessions use Parrot database format version `9` in SQLite, with 5 application
tables. Root identity, mode state, and permanent agent lineage live in `meta.json`.
Tool calls are stored as `tool_call` rows in `conversation_part`, alongside text
and image parts. Named history checkpoints are derived from valid `set_checkpoint`
calls and their successful results, without a separate checkpoint table. Context
reminders use the `context_reminder` conversation kind. Their checkpoint data stays
on the same record. Compaction summaries are typed conversation records, not
ordinary model messages. Each summary has a coverage watermark separate from its
insertion position. Usage history anchors use the conversation sequence, and
`history.jsonl` remains a derived file. SQLite's internal tables are not part of
the table count. Versions `1` through `8`, existing nonempty unversioned databases,
and unknown format versions are unsupported. The same format check applies in
both current and legacy filesystem roots.

By default, `parrot` and `parrot chat` load or join the workspace's latest session.
If resume encounters an unsupported database, startup reports the incompatibility
and does not create a replacement. There is no automatic migration, reset, or
session deletion.

To start a different local session explicitly, run:

```sh
parrot chat --new-session
```

This option skips discovery, attachment, and resume of the previous session. It
uses the existing create-session path and normal ownership checks. The previous
session's database and artifacts remain intact. `/clear` cannot resolve a refusal
that occurs before the interactive chat starts.

An older binary can continue to use its untouched old sessions after all current
owners have stopped. Opening a new-format session with an older binary is
unsupported. Keep complete session backups and follow the
[rollback checklist](#backup-and-rollback-checklist) when switching binaries.

Durable accounting uses individual recorded facts. Live aggregate usage
publications remain available, but aggregate SQL usage APIs are retired.
Explicit image claim and release APIs are also retired. Input and conversation
part references retain their images. Tool results and their execution status belong
to conversation records. Their text and image references are stored in conversation
parts, not separate execution-result tables.

Image parts own their inspected metadata in `input_part` and `conversation_part`.
Image files are named by their SHA-256 hash in the selected session's `artifacts`
directory. Uploads have no separate database identity. Message image references
carry their own display names, while admission inspects local bytes for content
metadata. Repeated bytes share one file without sharing reference labels. Session
opening removes expired files only when neither part table references them. See [image metadata ownership](../session-storage.md#image-metadata-belongs-to-its-part).

## Session identity metadata

The selected session's `meta.json` owns the stable root agent ID and name, current
mode, mode revision, and permanent agent lineage. Model selection, fast mode,
and the task-limit override remain in the same file. Keep this file with its
session database. Missing or corrupt identity metadata is an opening error, not
permission to generate a new root ID.

Lineage retains retired identities and successive occupants of a reused agent
name. Retiring an agent removes its current `agent-state.json`, not its permanent
lineage record. Lineage alone does not restore a retired agent or run its work.

SQLite stores each status assembly as one system row with a JSON record of the
slot hashes it stands for. Assemblies append only; reopening writes no duplicate
for unchanged hashes. Metadata replacement is not a transaction with SQLite or
the recovery snapshots.

To inspect the actual artifacts of a closed format-10 session, run from the source
checkout inside the development shell:

```sh
python3 scripts/verify-session-storage /absolute/path/to/session
```

The read-only check verifies the database format, all 5 application table names,
JSON identity and lineage, and the standing status slot records.
Add `--reused-path main/worker` to require multiple historical occupants of that
name-based directory.
See [session storage ownership](../session-storage.md) for the design rationale.

## Managed task concurrency

Set the shared extra-task budget in the new-root `config.yaml`:

```yaml
agent_tasks:
  extra_concurrent_tasks: 2
```

The default is `2`. Each owner has one free dedicated AgentTask run. Additional
dedicated runs across all owners in the same user session share the extra slots.
Self assignments use no child capacity. Setting `0` permits one self task plus
one free dedicated run per owner, including owners of nested graphs. Use an
unquoted integer between `0` and `2147483647`.

The limit applies only to new dedicated managed-task starts, including restarts
on retained workers. It does not limit direct `agent_spawn` calls or the total number of
agents. Tasks without capacity stay pending and start automatically when a slot
becomes available. Existing running tasks are not stopped, and internal retries
keep their admission.

Use `/task-limit` to read the current session's effective extra-task budget from
the host. Use `/task-limit N` to persist a session override, including
`/task-limit 0`. `N` must be one decimal integer from `0` to `2147483647`; there
is no reset-to-default command. The override is stored in the selected session's
`meta.json` and survives reopening, even if the global default changes. Sessions
without an override, including older sessions, use the configured default.
Querying does not write an override. Other sessions and global configuration are
unchanged.

Increasing the session limit wakes eligible pending work immediately. Decreasing
it does not stop running tasks: new extra starts wait until usage falls below the
new limit, and each owner's free start remains available. On session recovery,
saved running tasks resume even if usage exceeds the effective limit. The global
configuration is read when the session runtime is constructed, not automatically
reloaded into a running session; `/task-limit N` changes that session's live
budget only after its override has been persisted successfully.

## Retained agents and task recovery

The composed session runtime now persists retained agents and task orchestration.
This describes recovery **after a session owner is opened**; it does not remove
the local production-creation limitations noted above.

- Each agent owns one versioned `agent-state.json` beneath
  `root-agents/<root-name>/<child-name>/.../` in the selected session directory.
  The root has its own file under `root-agents/<root-name>/`. A parent stores
  direct-child name/ID references, not copies of the child's graph or snapshot.
  Do not move these files between agents or sessions, or edit their internal
  schema as if they were `set_agent_tasks` input artifacts.
- Opening follows the retained roster, preserving original IDs, parent links,
  histories and local prompt scopes, including idle/completed/never-started
  children. Old lineage events and leftover directories do not recreate obsolete
  incarnations. Idle or completed agents do not run solely because they reopen.
- Saved profile/model selectors use current configuration and normal
  parent-linked restrictions. No credentials or old live grants are restored.
  Removing a required selector causes an error rather than a silent substitute;
  changing policy can change what resumed work is allowed to do.
- Each owner recovers its saved graph, including results, failed/canceled states,
  hidden flags and assignments. A current-format dedicated attempt continues with
  its existing prompt identity, retry count and response-repair budget. A self
  assignment remains owner work rather than launching a child. Dependents
  remain held by failed dependencies. Reopening does not automatically reset a
  terminal task; an explicit task reset is a new transition with its normal
  dependent invalidation and old-generation cancellation behavior.
- Normal hosted close suspends unfinished work before resource cancellation.
  Explicit task cancellation or user interruption is still terminal, not a
  request to suspend. A crash can resume unfinished work too, but does not imply
  that the last uncommitted write survived.
- Work queues use the packaged `queue` skill and files beneath each owner's
  `scratch/queues/` directory. The runtime no longer attaches a native queue
  service to restored agents. Old queue files remain untouched and unused;
  see [File-based work queues](#file-based-work-queues).

Resuming a format-9 session requires its root snapshot. A missing root snapshot
fails opening instead of using root-only legacy recovery. An interrupted initial
creation can leave canonical identity metadata without a complete recovery
snapshot. Preserve that session and start a separate session explicitly rather
than deleting identity metadata to force creation.

Once a parent references a child, that child's missing snapshot also fails
opening. Corrupt or unsupported snapshots, wrong owner IDs, invalid parent
references, duplicate retained names, cyclic IDs, and unavailable selectors fail
opening before provider activation. Preserve the session and error for diagnosis.
Deleting snapshots to bypass the error loses recoverable state. No repair or
migration command is provided.

SQLite `session.db` remains authoritative for conversation/event history and
settlements; agent `history.jsonl` files remain projections. Back up the whole
selected session after closing its owner, including snapshots, DB and agent scratch
files, not just a graph file. Snapshot replacement is atomic within its agent directory,
**not a transaction with SQLite**. Stable input/result/notification identities
reconcile the file/DB handoffs and avoid resubmitting an already admitted initial
assignment. They do not promise exactly-once external tool effects: if a process
dies after an effect but before its terminal settlement commits, unresolved-tool
recovery may invoke it again. Do not treat resume as a rollback of external work.

### Assignment and accepted-response recovery

For each owner, the scheduler chooses one ready ordinary instruction leaf for
`self` in stable graph order. Composites and tasks with model overrides always
use dedicated workers. Other ready parallel work uses dedicated workers as
capacity permits. Existing dedicated runs are not reassigned when later self
work becomes ready. Later self handoffs arrive automatically as notifications.

A self assignment must record its outcome and result through `set_agent_tasks`.
Final chat or JSON does not complete it. A dedicated worker still completes its
parent task through strict final JSON. A composite worker registers its
provisional inner graph, performs and explicitly completes inner self tasks,
supervises dedicated work, and resolves the inner graph before its parent-facing
final JSON.

Both list and detail queries expose optional read-only `assigned_to`. It is
`self` or an absolute registered dedicated-agent path, is omitted for a task
never assigned, and retains the last assignee until reset. `agent_name` remains
the dedicated-worker compatibility alias. Neither field is accepted task input.
Non-hidden self tasks stay visible without a child link. Hidden self tasks stay
hidden. Temporary visibility applies only to active dedicated-worker activity,
using the worker ID.

New accepted updates save their rendered rich response before returning it.
Recovery replays those exact sealed bytes before rereading call arguments or a
mutable task artifact. Use `get_agent_tasks` and later notifications to inspect
current state rather than treating a replay as a fresh query. Legacy count-only
receipts cannot reconstruct historical rich-response bytes absent from their
snapshots; their original count response remains supported.

### AgentTask snapshot migration

**Back up the whole session before opening it with the new binary.** AgentTask
input artifacts still use schema version `1`. Retained recovery snapshots use
version `2`, with strict loading support for compatible version-`1` snapshots.
This is not a SQLite database-format migration.

Safe legacy graphs migrate once. Before reassignment, migration stops and joins
obsolete attempts and the nested work those attempts own. Terminal tasks and
retained histories are preserved. Unfinished work can be reassigned under the
new policy, so partial external work may repeat. Reopening a migrated graph does
not repeat that legacy conversion.

**An ambiguous legacy pending input blocks opening.** This refusal happens
before provider execution, cancellation, claim release, or consumption of that
input. Its original bytes and state remain intact. Preserve the session and
resolve that exact pending input externally with compatible tooling or a
compatible version before retrying the new binary. Do not fall back to executing
the old attempt, discard the input, match prompt text to guess its identity, or
bulk-clear pending inputs. There is no in-app migration repair command.

An older binary may reject version-`2` snapshots even when `session.db` has not
changed format. Do not edit version fields or attempt an in-place downgrade.
After stopping all owners, restore a complete consistent pre-migration backup
with its matching binary and workspace snapshot. Restoring state does not undo
external effects. Follow the [backup checklist](#backup-and-rollback-checklist).

## File-based work queues

Use the packaged [`queue` skill](../../assets/skills/queue/SKILL.md) for shared
producer/consumer work. Its standard-library helper requires Python 3 on Linux
or macOS and ordinary authorized command execution. The native package does not
bundle Python or grant additional file access.

Queue files live at `$AGENT_SCRATCH_DIR/queues/<name>.json`, beneath the owning
agent's `scratch/` directory. Give workers explicit absolute paths to those files;
a child's environment points to its own scratch directory, not its parent's.
Use the helper for reads and mutations so cooperating processes share its stable
file lock and atomic state replacement. Queue files survive close/reopen without
a native runtime queue service or file watcher. The owner or designated final
closer waits for all producers before closing a shared queue. An open empty queue
can still receive work; a closed empty queue is drained.

The owner refreshes one aggregate `progress` report from its selected queue files.
Set `steps` to items taken and `total_steps` to all items added, with the waiting
count in `current_step`. For ten items with seven taken, report `7/10` and
`3 waiting`, even if those seven items are still in flight. Multiple queues share
that owner's one report. Refresh after seeding or changing work, during normal
coordination waits, and before closing; counts are snapshots, not live file
subscriptions. If the total is zero, omit the update rather than send `0/0` or
invent a step. Keep coordinating an open empty queue.

After all selected queues close and drain, publish final counts and explicitly
close progress. Reaching 100% does not close it. Still join workers and check their
outcomes before declaring their work complete. On intentional stop, report the
remaining items, close progress explicitly, and leave queue files intact. Queue
files have no separate completion blocker; existing progress, child, task,
process, and reminder checks remain in force.

Old native queue files under an agent root's `queues/`, or an older session-level
`queues/`, stay untouched and unused. There is no automatic conversion or
reattachment. That agent-root directory may still hold process sandbox resources;
it is not the skill's queue directory. Historical queue tool results and snapshot
records remain readable, but the removed tools cannot execute and unresolved old
queue calls do not become skill operations. A take removes waiting items before
their work runs; there is no acknowledgement or automatic recovery of taken items.
Inspect an interrupted mutation and worker outcomes before retrying it.

## Agent progress

The `progress` tool reports completed `steps` against positive `total_steps`, with
an optional `current_step` description. Each agent has one report. An update
replaces its counts and description; counts may decrease or the total may change
when work is replanned. Omitting the description clears the previous one.

Active progress blocks that agent's normal completion, including at 100%. The
agent must call `progress` with `close: true` to close it. A close-only call needs
no counts, and closing below the total is allowed: it dismisses the report rather
than asserting that all work succeeded. Repeated closes are harmless. A later
update starts a new report. Closing progress does not clear goals, ordinary exit
reminders, or other active work, and `clear_exit_reminder` cannot close progress.

Each agent stores its versioned snapshot in its own directory:

- Root: `root-agents/<root-name>/progress.json`.
- Child: `root-agents/<root-name>/<child-name>/.../progress.json`.

These paths are relative to the selected session directory, not the project
workspace or the agent's `scratch/` subdirectory. The JSON contains
`schema_version`, `owner_session_id`, `revision`, and `progress`. An active
`progress` value contains `steps`, `total_steps`, and nullable `current_step`.
Closing writes `progress: null` while retaining the owner and revision. The
existing `agent-state.json` format is unchanged. An absent progress file means
inactive progress, so older sessions need no migration. Opening an agent that has
never reported progress does not create a file.

The enhanced CLI shows independent labeled bars for the main agent and reporting
children. Reattach or reopen restores the current bars without another progress
call. The basic CLI and web UI retain their generic tool presentation rather than
adding a progress widget. Interrupting an agent or closing the application does
not close its saved report. User interruption and shutdown remain available, but
the report blocks that agent's next normal completion until explicitly closed.
Saved progress alone does not activate a provider or resume an idle agent.

Tool allowlists and disabled-tool settings still apply. If `progress` is disabled
while an agent has an active saved report, re-enable it before expecting normal
completion. The runtime does not silently close the report or bypass the policy.

Snapshot replacement is atomic within the agent directory, using
`progress.json.tmp` as staging. A pre-commit error preserves the prior report. A
failure after rename can mean the new file is already visible; the runtime
reconciles from the file or fails closed rather than claiming rollback. Corrupt,
unsupported, or wrong-owner files are errors, not an implicit close. Preserve the
session and diagnostic instead of editing or deleting state to bypass the gate;
there is no automatic repair or orphan-directory recovery.

Back up progress files with the rest of the selected session after closing its
owner. They are independent of SQLite conversation and tool-settlement history,
not a transaction with it. Absolute-count replacements and closes can be retried,
but this does not promise exactly-once external tool effects.

## Ownership, takeover and concurrency

**Never use .NET and Rust concurrently on the same workspace**, even if their
session IDs or application roots differ. Rust admission checks ownership in both
roots before activation, including legacy owners, but these checks are not a
global lock against every process/version. **Existing .NET cannot observe
new-root Rust ownership records. No cross-version global locking or simultaneous
same-workspace safety is promised.** A .NET process started after Rust admission
can miss the Rust owner; different XDG roots or hosts make that limit still more
important. Establish exclusive use operationally before switching versions.

- An inactive, valid owner can be resumed under the selected-root rules.
- A verified live owner is not displaced by takeover. A009 also refuses a local
  live PID whose incomparable start identity cannot disprove the old owner; this
  is not overridable merely by passing the flag.
- An uncertain owner (for example another host or one the process cannot inspect)
  requires explicit **`parrot chat --take-over`** to reclaim. Investigate and ensure
  the former owner has stopped normally first. This flag is not a kill command,
  force-steal of verified-live ownership or bypass of corrupt/contended records.
- Corrupt or contradictory ownership/session metadata must be investigated, not
  deleted to force admission. Do not delete claim/socket files by hand while an
  owner may still be running.

Takeover does not bypass database-format checks. A remote connection or
existing-owner attach needs separate authorization and is not permission to
operate another process's workspace.

## Backup and rollback checklist

1. Close both implementations and their sessions **normally** and establish that
   no writer is using the workspace/state being backed up. Do not use takeover
   as a stopping mechanism; an ownership record alone is not a host-wide
   quiescence proof.
2. Before switching binaries or opening existing state, make a consistent backup
   of **both complete state roots**, not just `session.db`. Include ownership
   ledgers, session DBs and any SQLite journal/sidecar files, queues, artifacts,
   metadata, scratch, history projections and logs. Preserve config/data
   separately and snapshot workspace files that agent tools may modify. Protect backups as secret data.
   Copying a live SQLite file alone is not a consistent backup procedure.
3. Keep the exact older binary/package and note the selected paths. An isolated
   Nix profile lets you select an older executable without deleting newer output;
   it does **not** restore databases or workspace files.
4. An older binary can use its untouched old sessions after all current owners
   have stopped. Do not open new-format sessions with an older binary. AgentTask
   snapshot version `2` can be incompatible even if SQLite is unchanged. Never
   edit snapshot version fields. Database downgrade and transaction merging are
   unsupported. If you restore a backup,
   first retain a separate snapshot of the current state for recovery. Restore
   the complete consistent backup and corresponding workspace snapshot while
   both apps are closed. Restoring discards post-backup changes, so plan recovery
   explicitly.
5. Do not merge individual SQLite/history/queue files across versions or roots,
   point .NET at the Rust root, or treat setting the flag false as rollback.
   Validate recovery on an isolated copy before using the only original state.

See [installation limits](installation.md) before opening existing state.
