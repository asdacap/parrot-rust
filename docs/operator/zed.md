# Use Parrot as an ACP agent in Zed

Zed and other editors that speak the Agent Client Protocol (ACP) can drive
Parrot as their coding agent. The editor launches Parrot as a subprocess and
talks to it over JSON-RPC on stdio; you do not run `parrot serve` or open any
port for this.

Parrot implements ACP protocol v1. It exposes one session per editor session,
streams assistant messages and tool activity back as `session/update`
notifications, and asks the editor for permission decisions through
`session/request_permission`. The decision follows the same permission broker
the CLI and web use, and file reads and writes that the client advertises
`fs.readTextFile` or `fs.writeTextFile` for are performed by the editor, so the
editor's own permission and diff UI applies to them.

## Configure Zed

Open Agent Settings with `agent: open settings`, go to the External Agents
page, click Add Agent, and choose Add Custom Agent. Zed opens your settings
file with an `agent_servers` entry. Fill it in with the Parrot command. Zed
launches the command in the project directory, so `parrot` must be on the
editor's `PATH` or you must use an absolute path. Run
`parrot auth login <provider> --api-key-stdin` beforehand if you have not
stored credentials yet; the ACP process reads the same configuration and
credential store as `parrot chat`.

```json
{
  "agent_servers": {
    "parrot": {
      "type": "custom",
      "command": "parrot",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

The `parrot acp` subcommand takes no flags. Keep the field as a plain command
and argument list; anything else is a usage error.

Zed starts the process when you select the Parrot agent and closes it with the
session. If the process exits immediately, run `parrot acp` in a terminal and
press Enter to end stdin; a clean exit code means the binary itself is fine and
the problem is in how Zed resolved the command. Use `dev: open acp logs` in
Zed's Command Palette to inspect the messages between Zed and Parrot.

## What is supported

- `initialize` with protocol v1.
- `session/new` with Parrot's configured default model and mode.
- `session/prompt` with text content.
- Streaming updates: assistant message chunks, reasoning chunks, and tool call
  progress with completed and failed statuses.
- `session/request_permission` with allow and reject decisions.
- `session/cancel`.
- `fs/read_text_file` and `fs/write_text_file` performed by the editor when it
  advertises them.

## What is not supported

Session modes, `session/load`, `session/resume`, `session/close`, slash
commands, and attachment upload are not part of this version. An editor session
always starts fresh; resume your work with `parrot chat` instead. Editor
sessions also cannot collect a reason for permission choices that require one.
