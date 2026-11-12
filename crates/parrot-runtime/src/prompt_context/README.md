# Prompt context and skills

Prompt context combines configured system prompts, model guidance, identity,
standing instructions, AGENTS files, environment information, and skill metadata.
Descriptions of permissions remain prompt text and never grant authority.

## Prompt sections and refresh

System-prompt keys must be valid namespaced keys. Duplicate keys are rejected.
Sections use UTF-16 ordinal ordering. Configured nonblank text retains its
original content, including whitespace. Model guidance combines alias
descriptions, selected model-profile guidance, and selected-alias augmentation
in that order.

Standing-instruction changes are visible to already prepared prompt views. Local
prompt-scope history records effective changes in order. Repeating the last scope
or supplying an empty scope inherits it without adding another change. Prompt
scope is distinct from security ancestry.

AGENTS files follow root-to-working-directory ancestry and unique global-config
precedence. A new sampling epoch replaces the stored view only after all reads
succeed. Missing files are distinct from denied, corrupt, or changed-file errors.
Denied reads do not fall back to unrestricted filesystem access. AGENTS text uses
BOM-aware replacement decoding. Sampling does not expand templates.

Live prompt rendering does not itself resample AGENTS files. Template failures
occur during rendering rather than metadata lookup or sampling. A failed or
canceled refresh preserves the prior stored view but returns the failure to its
caller instead of silently presenting stale data as a successful refresh.

Prompt sampling, input acquisition, and rendering are serialized with each other.
That serialization does not extend across provider, tool, status, or persistence
operations.

## Environment information

Displayed paths use the selected session resources and agent scratch identity.
Path compression prefers longer parents and respects literal environment
overrides. The platform identifier reflects the native operating system and
architecture. Available command names are sorted ordinally. The expected `bwrap`
entry is suppressed on macOS. Git context uses the resolved project workspace.
These displayed values do not authorize filesystem or command access.

## Skill discovery

Skill discovery searches roots in nearest-repository-ancestor, user, then packaged
order. Discovery proceeds breadth first with UTF-16 ordinal ordering. It has a
maximum depth of 6, a 2,000-directory bound, and a 20,000-entry bound. Hidden
directories, including dot-prefixed directories, are not traversed. Permitted
directory links are cycle-checked. Discovery rejects disallowed system-root links
and file links. Canonical physical paths deduplicate discovered skills.

Skill reads require strict UTF-8 and a target-bound 1 MiB limit. Frontmatter and
optional display YAML use strict parsing with bounded metadata. Skill lookup
accepts canonical paths, discovery paths, relative paths, and supported macOS
aliases. Mention parsing retains UTF-16 positions and recognizes mentions inside
code and backslash text.

Metadata lookup does not read instruction bodies. Unknown references can refresh
the catalog once per lookup batch. Known disabled records do not trigger that
refresh. The refreshed metadata is visible to later reads in the same prompt
epoch. Body reads use fresh authorization against the expected physical target.
Unavailable reads return diagnostics rather than an invented skill body.

## Turn-scoped skill context

A turn captures its skill catalog. Selected skill identities are deduplicated
before their bodies are read, and loaded content is ordered consistently. The
selected-file limit is 8 KiB and the combined prompt limit is 1 MiB. The rendered
catalog has a 64 KiB budget. Ending a turn releases its captured view. An idle
prompt observation acquires a current catalog rather than keeping an old turn's
copy.

## Packaged skill installation

Bundled skill files preserve their relative paths, bytes, and executable intent.
Installation must finish before its resources are released. Dropping an observer
does not prove that installation has completed.

A missing or different bundle `VERSION` marker refreshes every bundled target,
including local edits. An unchanged bundle preserves edits and restores missing
files. Custom files remain untouched. The marker is published only after all
bundled files succeed. The bundle identity includes paths, bytes, and executable
intent, so no manually maintained source version is required.

## Capability limits

Prompt context requires authorized reads and installation. Inert descriptions and
resolved-path reports do not prove filesystem enforcement. Configuration and
credentials do not gain legacy fallback from prompt discovery. Known blocked
captured-file approval widening remains unresolved by this component.

Component tests of discovery, prompts, or injected services do not establish
production host authorization or full runtime acceptance. Linux-hosted platform
cases are not native Apple Silicon validation. Native-platform, security, and
release gates remain separate.

The real alias catalog rejects some missing-provider or empty-usage states that
older source tests constructed directly. Supported fallback and refresh behavior
does not claim same-input equivalence for those unrepresentable states.
