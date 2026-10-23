# Configuration, legacy state and rollback

**A legacy resume is write-capable, not a read-only import.** With the default
compatibility setting enabled, selecting an old session can open and **mutate its
old `session.db` and directory in place**. Back up before the first Rust write,
not after the first prompt. No bulk migration, automatic root copy, merge or
schema downgrade/rollback command is provided.

Current local session creation/resume requiring a new local ParrotService remains
unavailable/fail-closed. The rules below document the approved selected-root
contract and implemented components; they do not claim the missing end-to-end
production capability is available. Attachment to an already running owner may
still affect that owner's session; an unavailable local creator is not a backup
strategy.

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

## Default setting and disabling compatibility

The top-level YAML boolean **`legacy_path_read_fallback` defaults to `true`**.
Despite its name, enabled legacy-session resume can write old state in place.
To disable compatibility discovery/read fallback, edit the new-root `config.yaml`
(top-level key, not a CLI switch; do not append a duplicate key):

```yaml
legacy_path_read_fallback: false
```

This disables future old-session discovery and compatibility read fallback. It
is **not** a filesystem deny rule for explicit, normally authorized access to an
old pathname. It does not undo earlier writes, relocate/close an already selected
session, or split its resources. Ownership safety still checks both current and
legacy roots even when discovery fallback is disabled.

## Selection, read precedence and no resource mixing

| Operation | Behavior |
| --- | --- |
| Fresh session | Always new state root. No automatic migration. |
| Existing session / duplicate ID | Select the new-root session directory if present; only if absent and fallback enabled consider the legacy directory. Never merge duplicates. |
| Selected legacy session | Resume and write its existing old-root DB/resources **in place**. It does not become a new-root session. |
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
  hidden flags and worker associations. An unfinished attempt continues with its
  existing prompt identity, retry count and response-repair budget. Dependents
  remain held by failed dependencies. Reopening does not automatically reset a
  terminal task; an explicit task reset is a new transition with its normal
  dependent invalidation and old-generation cancellation behavior.
- Normal hosted close suspends unfinished work before resource cancellation.
  Explicit task cancellation or user interruption is still terminal, not a
  request to suspend. A crash can resume unfinished work too, but does not imply
  that the last uncommitted write survived.
- Existing child queue files under `queues/agents/<agent-session-id>` survive
  close/reopen and reattach to restored owners. Leftover queue files alone do not
  authorize or restore an obsolete agent.

An existing session **without a root snapshot** uses root-only legacy recovery
and emits a visible `legacy_root_only_resume` notice (text configured by
`session.legacy-resume-warning`). It begins writing new snapshots, but cannot
reconstruct historical child/task runtime state that was never saved. Fresh
sessions do not emit this warning. This snapshot compatibility case is distinct
from the old-path fallback setting: it can also apply to an older Rust session in
the current root.

Once a parent references a child, that child's missing snapshot is damaged
new-format state, **not** a root-only fallback. Corrupt/unsupported snapshots,
wrong owner IDs, invalid parent references, duplicate retained names/cyclic IDs,
and unavailable selectors fail opening before provider activation. Preserve the
session and error for diagnosis; deleting snapshots to bypass the error loses
recoverable state. No repair/migration command is provided.

SQLite `session.db` remains authoritative for conversation/event history and
settlements; agent `history.jsonl` files remain projections. Back up the whole
selected session after closing its owner, including snapshots, DB and queues,
not just a graph file. Snapshot replacement is atomic within its agent directory,
**not a transaction with SQLite**. Stable input/result/notification identities
reconcile the file/DB handoffs and avoid resubmitting an already admitted initial
assignment. They do not promise exactly-once external tool effects: if a process
dies after an effect but before its terminal settlement commits, unresolved-tool
recovery may invoke it again. Do not treat resume as a rollback of external work.

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

Takeover describes admission behavior, **not a currently working local resume
recipe**: the required local production service is still unavailable. A remote
connection or existing-owner attach likewise needs separate authorization and is
not permission to operate another process's workspace.

## Backup and rollback checklist

1. Close both implementations and their sessions **normally** and establish that
   no writer is using the workspace/state being backed up. Do not use takeover
   as a stopping mechanism; an ownership record alone is not a host-wide
   quiescence proof.
2. Before any Rust use of old state, make a consistent backup of **both complete
   state roots**, not just `session.db`: include ownership ledgers, session DBs
   and any SQLite journal/sidecar files, queues, artifacts, metadata, scratch,
   history projections and logs. Preserve config/data separately and snapshot
   workspace files that agent tools may modify. Protect backups as secret data.
   Copying a live SQLite file alone is not a consistent backup procedure.
3. Keep the exact older binary/package and note the selected paths. An isolated
   Nix profile lets you select an older executable without deleting newer output;
   it does **not** restore databases or workspace files.
4. If returning before Rust has written state, retain the untouched backup and
   resume only after all Rust owners are closed. After Rust has written an old
   DB/session, do **not** assume .NET can read/reverse every mutation. There is no
   automatic schema downgrade or transaction merger. Retain a separate snapshot
   of the post-Rust state for recovery, then restore the complete consistent
   pre-Rust backup and corresponding workspace snapshot while both apps are
   closed. This discards post-backup changes; plan recovery explicitly.
5. Do not merge individual SQLite/history/queue files across versions or roots,
   point .NET at the Rust root, or treat setting the flag false as rollback.
   Validate recovery on an isolated copy before using the only original state.

See [installation limits](installation.md) before opening existing state.
