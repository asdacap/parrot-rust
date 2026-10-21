# Parrot — experimental native Rust port

Native port of Parrot pinned to .NET source commit
`5f27a8a92625ee31d8bec094e5c3e09550193781`. The executable remains `parrot`;
the retained browser frontend, protocols, prompts, skills and Linux C PTY helper
are part of the project. The .NET application is a development reference, not a
native runtime dependency.

**Not a production-ready or release-accepted replacement.** Local session
creation/resume requiring a new local service, `serve` and `web` are currently
**UNAVAILABLE / fail-closed**: the concrete production security/capability bundle
and local ParrotService are missing. Existing-owner attachment and explicit remote
`chat --connect` are experimental interfaces, not proof of complete local operation.
Do not substitute an unrestricted or test/denying backend.

**Before using existing state:** resuming a selected legacy session can **mutate
its old database and session directory in place**. Back up before any Rust write.
Never run .NET and Rust against the same workspace concurrently. Existing .NET
cannot observe ownership records in the new Rust root, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)
