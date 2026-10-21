import { create, toBinary, type MessageInitShape } from "@bufbuild/protobuf"
import { describe, expect, it } from "vitest"

import { AgentTaskProgressSnapshotSchema, EventSchema, ProviderRequestPhase, ReasoningKind } from "@/gen/parrot_pb"
import { emptyTimeline, reduceTimeline, type TimelineState } from "@/session/timeline"

type Payload = MessageInitShape<typeof EventSchema>["payload"]

const root = "root-agent"
const child = "child-agent"

function taskProgress(progress: MessageInitShape<typeof AgentTaskProgressSnapshotSchema>): Payload {
  const data = toBinary(AgentTaskProgressSnapshotSchema, create(AgentTaskProgressSnapshotSchema, progress))
  return { case: "toolSnapshot", value: { kind: "agent_task_progress", revision: progress.revision, finalChunk: true, data } }
}

function replay(events: [agentSessionId: string, payload: Payload][]): TimelineState {
  return events.reduce(
    (state, [agentSessionId, payload]) =>
      reduceTimeline(state, { type: "event", event: create(EventSchema, { agentSessionId, payload }) }),
    emptyTimeline,
  )
}

describe("reduceTimeline", () => {
  it.each<[string, [string, Payload][], object]>([
    [
      "concatenates streamed text until another item from the same agent intervenes",
      [
        [root, { case: "textChunk", value: { fragment: "Hel" } }],
        [child, { case: "reasoningChunk", value: { fragment: "thinking", kind: ReasoningKind.SUMMARY } }],
        [root, { case: "textChunk", value: { fragment: "lo" } }],
        [root, { case: "toolStarted", value: { toolCallId: "call-1", toolName: "read" } }],
        [root, { case: "textChunk", value: { fragment: "Done" } }],
      ],
      {
        items: [
          { kind: "assistant", agentSessionId: root, text: "Hello" },
          { kind: "reasoning", agentSessionId: child, text: "thinking", completed: false },
          { kind: "tool", agentSessionId: root, toolCallId: "call-1", name: "read", args: "", status: "running", output: "" },
          { kind: "assistant", agentSessionId: root, text: "Done" },
        ],
      },
    ],
    [
      "tracks a tool call from argument chunks to its result in a single card",
      [
        [root, { case: "toolCallChunk", value: { toolCallId: "call-1", toolName: "exec", argumentsFragment: '{"cmd":' } }],
        [root, { case: "toolCallChunk", value: { toolCallId: "call-1", argumentsFragment: '"ls"}' } }],
        [root, { case: "toolStarted", value: { toolCallId: "call-1", toolName: "exec" } }],
        [root, { case: "toolFinished", value: { toolCallId: "call-1", toolName: "exec", result: "a.txt" } }],
        [root, { case: "toolStarted", value: { toolCallId: "call-2", toolName: "write" } }],
        [root, { case: "toolError", value: { toolCallId: "call-2", toolName: "write", message: "denied" } }],
      ],
      {
        items: [
          { kind: "tool", agentSessionId: root, toolCallId: "call-1", name: "exec", args: '{"cmd":"ls"}', status: "finished", output: "a.txt" },
          { kind: "tool", agentSessionId: root, toolCallId: "call-2", name: "write", args: "", status: "error", output: "denied" },
        ],
      },
    ],
    [
      "ends a busy turn on failure, keeping the provider's response",
      [
        [root, { case: "turnStarted", value: { model: "gpt" } }],
        [root, { case: "turnFailed", value: { message: "rate limited", providerResponseBody: "429" } }],
      ],
      {
        busy: false,
        items: [{ kind: "notice", agentSessionId: root, tone: "error", text: "rate limited", detail: "429" }],
      },
    ],
    [
      "reports how the main agent's turn ended, and nothing for a subagent's",
      [
        [child, { case: "agentStarted", value: { parentAgentSessionId: root, name: "helper" } }],
        [child, { case: "turnEnded", value: { finishReason: "stop" } }],
        [child, { case: "agentFinished", value: { parentAgentSessionId: root, name: "helper", elapsedMs: 65_000n } }],
        [root, { case: "turnEnded", value: { finishReason: "interrupted" } }],
      ],
      {
        items: [
          { kind: "notice", agentSessionId: child, tone: "active", text: "agent helper started" },
          { kind: "notice", agentSessionId: child, tone: "success", text: "agent helper finished (1m 05s)" },
          { kind: "notice", agentSessionId: root, tone: "error", text: "agent interrupted" },
        ],
      },
    ],
    [
      "keeps only reasoning summaries, starting a new block after a completed one",
      [
        [root, { case: "reasoningChunk", value: { fragment: "raw", kind: ReasoningKind.RAW } }],
        [root, { case: "reasoningChunk", value: { fragment: "one", kind: ReasoningKind.SUMMARY, completed: true } }],
        [root, { case: "reasoningChunk", value: { fragment: "two", kind: ReasoningKind.SUMMARY } }],
      ],
      {
        items: [
          { kind: "reasoning", agentSessionId: root, text: "one", completed: true },
          { kind: "reasoning", agentSessionId: root, text: "two", completed: false },
        ],
      },
    ],
    [
      "drops a canceled input and keeps the newest task progress",
      [
        [root, { case: "inputAdmitted", value: { inputId: "input-1", content: "first" } }],
        [root, { case: "inputAdmitted", value: { inputId: "input-2", content: "second" } }],
        [root, { case: "inputCanceled", value: { inputId: "input-1" } }],
        [root, taskProgress({ revision: 2n, rootNodes: [{ name: "new" }] })],
        [root, taskProgress({ revision: 1n, rootNodes: [{ name: "old" }] })],
      ],
      {
        items: [{ kind: "user", inputId: "input-2", text: "second" }],
        taskProgress: new Map([[root, { revision: 2n, rootNodes: [{ name: "new" }] }]]),
      },
    ],
    [
      "stays busy until the root mode turn completes, regardless of turn ends",
      [
        [root, { case: "turnStarted", value: {} }],
        [child, { case: "agentStarted", value: { parentAgentSessionId: root, name: "helper" } }],
        [child, { case: "turnStarted", value: {} }],
        [child, { case: "turnEnded", value: { finishReason: "stop" } }],
        [child, { case: "modeTurnCompleted", value: {} }],
        [root, { case: "turnEnded", value: { finishReason: "stop" } }],
      ],
      { busy: true },
    ],
    [
      "is idle once the root mode turn completes even while a subagent keeps working",
      [
        [child, { case: "agentStarted", value: { parentAgentSessionId: root, name: "helper" } }],
        [root, { case: "turnStarted", value: {} }],
        [child, { case: "turnStarted", value: {} }],
        [root, { case: "turnEnded", value: { finishReason: "stop" } }],
        [root, { case: "modeTurnCompleted", value: {} }],
      ],
      { busy: false },
    ],
    [
      "keeps the main agent's plan pending until later input, ignoring a subagent's plan",
      [
        [child, { case: "agentStarted", value: { parentAgentSessionId: root, name: "helper" } }],
        [root, { case: "planCompleted", value: { markdown: "# Plan", dialog: { prompt: "Proceed?" } } }],
        [child, { case: "planCompleted", value: { markdown: "# Child", dialog: { prompt: "Child?" } } }],
      ],
      { pendingPlan: { markdown: "# Plan" } },
    ],
    [
      "keeps the main agent's plan after its turn ends, ignoring its own lifecycle notices",
      [
        [root, { case: "agentStarted", value: { name: "main" } }],
        [root, { case: "turnStarted", value: {} }],
        [root, { case: "turnEnded", value: { finishReason: "stop" } }],
        [root, { case: "agentFinished", value: { name: "main" } }],
        [root, { case: "planCompleted", value: { markdown: "# Plan", dialog: { prompt: "Proceed?" } } }],
        [root, { case: "modeTurnCompleted", value: {} }],
      ],
      { busy: false, agents: new Map(), pendingPlan: { markdown: "# Plan" } },
    ],
    [
      "settles a pending plan once the main agent admits new input",
      [
        [root, { case: "planCompleted", value: { markdown: "# Plan", dialog: { prompt: "Proceed?" } } }],
        [root, { case: "inputAdmitted", value: { content: "go" } }],
      ],
      { pendingPlan: undefined },
    ],
    [
      "follows the main agent's request, tools and thinking, ignoring a subagent's",
      [
        [child, { case: "agentStarted", value: { parentAgentSessionId: root, name: "helper" } }],
        [root, { case: "turnStarted", value: { modelAliasIcon: { glyph: "*" } } }],
        [root, { case: "providerRequestPhaseChanged", value: { phase: ProviderRequestPhase.REQUESTING, attempt: 2 } }],
        [root, { case: "reasoningChunk", value: { fragment: "abcd", kind: ReasoningKind.RAW } }],
        [root, { case: "toolStarted", value: { toolCallId: "call-1", toolName: "read" } }],
        [root, { case: "toolStarted", value: { toolCallId: "call-2", toolName: "glob" } }],
        [child, { case: "toolStarted", value: { toolCallId: "call-3", toolName: "edit" } }],
        [root, { case: "toolFinished", value: { toolCallId: "call-2", toolName: "glob" } }],
      ],
      {
        modelIcon: { glyph: "*" },
        activity: { requestAttempt: 2, waitingForFirstToken: false, tools: [{ toolCallId: "call-1", name: "read" }], thinkingCharacters: 4 },
      },
    ],
    [
      "never replaces usage with an older revision",
      [
        [root, { case: "sessionUsageSnapshot", value: { revision: 2n, inputTokens: 20n } }],
        [root, { case: "sessionUsageSnapshot", value: { revision: 1n, inputTokens: 10n } }],
      ],
      { usage: { revision: 2n, inputTokens: 20n } },
    ],
  ])("%s", (_name, events, expected) => {
    expect(replay(events)).toMatchObject(expected)
  })
})
