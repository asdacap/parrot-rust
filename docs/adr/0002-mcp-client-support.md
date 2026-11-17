# 0002: MCP client support connects at user-session startup

Date: 2026-11-16
Status: Accepted

## Context

Parrot needs to call tools that external MCP (Model Context Protocol) servers
expose. Parrot has no MCP code today. The commented `mcp:` template in
`assets/config/predefined_config.runtime.yaml` documents a planned config shape,
and the runtime already owns the building blocks both transports need: tokio
process spawning with lifetime-linked shutdown, reqwest with rustls plus an
insecure-per-authority client pattern, and an in-tree SSE parser.

Tool definitions in the agent-session catalog are static. Every tool factory
requires its definition to exist in config `tools:`, but an MCP server's tool
list arrives at runtime over the protocol. The catalog, permission filtering,
and settlement paths already work by tool name, so dynamic names can flow
through them unchanged.

## Decision

1. Client only. Parrot connects to external MCP servers. It does not expose its
   own tools over MCP.
2. Tools only. The client implements the initialize handshake, `tools/list`,
   and `tools/call`. Resources, prompts, and sampling are out of scope.
3. Both transports: stdio and streamable HTTP, matching the commented template.
4. Configured servers are trusted. A server runs as an unsandboxed child with
   the user's privileges, and its tool calls skip per-call checks. The existing
   name-based `disabled_tools` and profile `allowed_tools` filters still apply.
5. The client is hand-rolled in a new `parrot-mcp` crate. Rejected
   alternative: the `rmcp` SDK that Codex pins from a git revision. This
   workspace pins every dependency exactly and builds offline under Nix, so a
   git dependency would need `outputHashes` packaging changes. The tools-only
   protocol surface is small, and the SSE, HTTP, and process foundations
   already exist in-tree. Codex's client sources remain the protocol reference.
6. MCP tool names are namespaced as `mcp__<server>__<tool>` to avoid collisions
   with built-in tools and between servers.
7. The manager connects each enabled server once at user-session startup,
   bounded by `startup_timeout_ms`. There is no auto-reconnect; a server that
   dies recovers on the next session start. There is no event-based lifecycle
   coordination.

## Consequences

- A configured MCP server can do whatever the user can do. Server config
  deserves the same care as shell configuration.
- The handshake blocks user-session startup up to `startup_timeout_ms` per
  server.
- A dead server surfaces as a clear failure on tool call and as absent tools;
  recovery needs a new user session.
- Tool output maps text content blocks to the existing text result. Image and
  resource blocks render as descriptive failure text.
- `Factory.name` becomes an owned `String` because MCP tool names are dynamic.
  Name uniqueness stays validated in `ToolSnapshot::document`, the single
  enforcement point.
