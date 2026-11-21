# 0003: ACP enters as an adapter boundary, the internal proto stays

Date: 2026-11-21
Status: Accepted

## Context

The proposal was to let the CLI talk to the core over ACP (Agent Client
Protocol) to reduce duplication. The code contradicts that premise. Every
frontend already shares one wire protocol: `proto/parrot.proto` defines 37 RPCs
including `SendMessage`, the `Listen` event stream, and `ReplyPermission`, and
`parrot-host` serves it as gRPC. Local CLI chat runs over a loopback gRPC Unix
socket (`crates/parrot-host/src/production/mod.rs:354`), and remote attach and
the web frontend use the same `ParrotClient`. There is no CLI-to-core protocol
duplication for ACP to remove.

ACP also cannot replace the internal proto losslessly. It has no concepts for
model presets, skills, slash commands, compaction, attachment upload, or
session resume, all of which the proto serves today. A migration would touch
every frontend for no deduplication gain.

The genuine opportunity is additive. ACP editors such as Zed spawn coding
agents as subprocesses and drive them over JSON-RPC on stdio. Serving that
interface lets those editors use Parrot without changing anything existing.

## Decision

1. The internal gRPC proto stays the single protocol between Parrot's core and
   its own frontends. ACP never becomes an internal transport.
2. Parrot exposes an ACP v1 agent adapter for external editor clients. A new
   `parrot acp` subcommand runs the adapter as a subprocess of the editor,
   newline-delimited JSON-RPC on stdin and stdout, using the
   `agent-client-protocol` crate, the same types Zed uses.
3. The first cut covers `initialize`, `session/new`, `session/prompt` with
   `session/update` streaming, `session/request_permission`, `session/cancel`,
   and the `fs/read_text_file` and `fs/write_text_file` client calls. Session
   modes, load, resume, and close are unsupported and unadvertised.
4. The adapter reuses the local host composition the CLI local chat path uses,
   so config, auth, and session wiring stay shared rather than duplicated.
5. The adapter pins ACP v1. ACP v2 is in preview and excluded.

## Consequences

- Editor users can drive Parrot from Zed and other ACP clients. Parrot's own
  frontends are unchanged.
- The adapter owns stdout exclusively. Any stray output on stdout corrupts the
  JSON-RPC stream, so logs go to stderr.
- Editor sessions have a reduced feature set compared to the CLI and web:
  no modes, no resume, no slash commands in this cut. The operator
  documentation states the gap.
- A later protocol migration to ACP would still be lossy for Parrot's feature
  set; this ADR is the recorded reason not to attempt it.
