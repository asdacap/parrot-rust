import { fromBinary } from "@bufbuild/protobuf"

import {
  AgentTaskProgressSnapshotSchema,
  ProviderRequestPhase,
  ReasoningKind,
  type AgentTaskProgressSnapshot,
  type Event,
  type PlanCompleted,
  type PlanTaskDeclaration,
  type SessionUsageSnapshot,
  type ToolSnapshot,
  type TurnModelAliasIcon,
} from "@/gen/parrot_pb"
import { ShellProcessSnapshotSchema, type ActiveShellProcess } from "@/gen/process_pb"
import { QueueSnapshotSchema, type QueueState } from "@/gen/queue_pb"
import { formatDuration } from "@/lib/duration"
import { emptyInventory, observeInventory, type OwnerInventory } from "@/session/inventory"

export type ToolStatus = "running" | "finished" | "cancelled" | "error"
export type NoticeTone = "muted" | "active" | "success" | "error"

export type TimelineItem =
  | { kind: "user"; agentSessionId: string; inputId: string; text: string }
  | { kind: "assistant"; agentSessionId: string; text: string }
  | { kind: "reasoning"; agentSessionId: string; text: string; completed: boolean }
  | {
      kind: "tool"
      agentSessionId: string
      toolCallId: string
      name: string
      args: string
      status: ToolStatus
      output: string
    }
  | { kind: "notice"; agentSessionId: string; tone: NoticeTone; text: string; detail: string }
  | { kind: "plan"; agentSessionId: string; markdown: string; taskDeclarations: PlanTaskDeclaration[] }
  | { kind: "print"; lines: string[] }
  | { kind: "error"; message: string }

export type ToolItem = Extract<TimelineItem, { kind: "tool" }>

export interface AgentInfo {
  name: string
  parentAgentSessionId: string
}

// What the main agent is doing now, for the status bar, following the terminal CLI's modeline.
export interface Activity {
  requestAttempt: number
  waitingForFirstToken: boolean
  // Running tools by call id, most recent last.
  tools: { toolCallId: string; name: string }[]
  // Raw reasoning received since the last answer text, which the terminal CLI shows as "Thinking (N tokens)…".
  thinkingCharacters: number
  compacting: boolean
}

const idle: Activity = { requestAttempt: 0, waitingForFirstToken: false, tools: [], thinkingCharacters: 0, compacting: false }

export interface TimelineState {
  items: TimelineItem[]
  // Every subagent seen; an agent session not in it is the main agent.
  agents: ReadonlyMap<string, AgentInfo>
  busy: boolean
  activity: Activity
  modelIcon: TurnModelAliasIcon | undefined
  usage: SessionUsageSnapshot | undefined
  // The main agent's plan still waiting for the user's decision; any later input settles it.
  pendingPlan: PlanCompleted | undefined
  // The newest task graph per agent session.
  taskProgress: ReadonlyMap<string, AgentTaskProgressSnapshot>
  queues: ReadonlyMap<string, OwnerInventory<QueueState>>
  processes: ReadonlyMap<string, OwnerInventory<ActiveShellProcess>>
}

export type TimelineAction =
  | { type: "event"; event: Event }
  | { type: "print"; lines: string[] }
  | { type: "error"; message: string }
  | { type: "planSettled" }
  // A new event stream; its usage snapshot starts a new revision sequence.
  | { type: "connected" }

export const emptyTimeline: TimelineState = {
  items: [],
  agents: new Map(),
  busy: false,
  activity: idle,
  modelIcon: undefined,
  usage: undefined,
  pendingPlan: undefined,
  taskProgress: new Map(),
  queues: new Map(),
  processes: new Map(),
}

// How deep an agent sits below the main agent; the main agent is 0.
export function agentDepth(agents: ReadonlyMap<string, AgentInfo>, agentSessionId: string): number {
  let depth = 0
  for (let current = agents.get(agentSessionId); current; current = agents.get(current.parentAgentSessionId)) {
    depth++
    if (depth > agents.size) break
  }
  return depth
}

function appendStreamed(state: TimelineState, event: Event, kind: "assistant" | "reasoning", fragment: string, completed = false): TimelineState {
  const lastIndex = state.items.findLastIndex(
    (item) => "agentSessionId" in item && item.agentSessionId === event.agentSessionId,
  )
  const last = state.items[lastIndex]
  if (last?.kind === "assistant" && kind === "assistant") {
    return { ...state, items: state.items.with(lastIndex, { ...last, text: last.text + fragment }) }
  }
  if (last?.kind === "reasoning" && kind === "reasoning" && !last.completed) {
    return { ...state, items: state.items.with(lastIndex, { ...last, text: last.text + fragment, completed }) }
  }
  const created: TimelineItem =
    kind === "assistant"
      ? { kind, agentSessionId: event.agentSessionId, text: fragment }
      : { kind, agentSessionId: event.agentSessionId, text: fragment, completed }
  return { ...state, items: [...state.items, created] }
}

function updateTool(
  state: TimelineState,
  event: Event,
  toolCallId: string,
  update: (tool: ToolItem) => Partial<ToolItem>,
): TimelineState {
  const index = state.items.findLastIndex((item) => item.kind === "tool" && item.toolCallId === toolCallId)
  const existing = state.items[index]
  if (existing?.kind === "tool") {
    return { ...state, items: state.items.with(index, { ...existing, ...update(existing) }) }
  }
  const created: ToolItem = {
    kind: "tool",
    agentSessionId: event.agentSessionId,
    toolCallId,
    name: "",
    args: "",
    status: "running",
    output: "",
  }
  return { ...state, items: [...state.items, { ...created, ...update(created) }] }
}

function append(state: TimelineState, item: TimelineItem): TimelineState {
  return { ...state, items: [...state.items, item] }
}

function notice(state: TimelineState, event: Event, text: string, tone: NoticeTone = "muted", detail = ""): TimelineState {
  return append(state, { kind: "notice", agentSessionId: event.agentSessionId, tone, text, detail })
}

function withInventory<T>(
  inventories: ReadonlyMap<string, OwnerInventory<T>>,
  owner: string,
  observe: (current: OwnerInventory<T>) => OwnerInventory<T>,
): ReadonlyMap<string, OwnerInventory<T>> {
  if (!owner) return inventories
  const current = inventories.get(owner) ?? emptyInventory
  const next = observe(current)
  return next === current ? inventories : new Map(inventories).set(owner, next)
}

function reduceToolSnapshot(state: TimelineState, event: Event, snapshot: ToolSnapshot): TimelineState {
  const chunk = { instance: snapshot.inventoryInstanceId, revision: snapshot.revision, index: snapshot.chunkIndex, final: snapshot.finalChunk, removed: snapshot.removed }
  switch (snapshot.kind) {
    case "agent_task_progress": {
      const progress = fromBinary(AgentTaskProgressSnapshotSchema, snapshot.data)
      const current = state.taskProgress.get(event.agentSessionId)
      return current && current.revision >= progress.revision
        ? state
        : { ...state, taskProgress: new Map(state.taskProgress).set(event.agentSessionId, progress) }
    }
    case "queue": {
      const queues = fromBinary(QueueSnapshotSchema, snapshot.data)
      return {
        ...state,
        queues: withInventory(state.queues, snapshot.ownerAgentSessionId, (owner) =>
          observeInventory(owner, { ...chunk, shape: queues.rootAgentSessionId, items: queues.queues }),
        ),
      }
    }
    case "shell_process": {
      const processes = fromBinary(ShellProcessSnapshotSchema, snapshot.data)
      return {
        ...state,
        processes: withInventory(state.processes, snapshot.ownerAgentSessionId, (owner) =>
          observeInventory(owner, { ...chunk, shape: "", items: processes.processes }),
        ),
      }
    }
    default:
      return state
  }
}

function reduceActivity(activity: Activity, event: Event): Activity {
  const payload = event.payload
  switch (payload.case) {
    case "turnStarted":
    case "turnEnded":
    case "turnFailed":
      return idle
    case "providerRequestPhaseChanged":
      return {
        ...activity,
        waitingForFirstToken: payload.value.phase === ProviderRequestPhase.HEADERS_RECEIVED,
        requestAttempt: payload.value.phase === ProviderRequestPhase.REQUESTING ? Math.max(1, payload.value.attempt) : 0,
      }
    case "toolStarted":
      return { ...activity, tools: [...activity.tools, { toolCallId: payload.value.toolCallId, name: payload.value.toolName }] }
    case "toolFinished":
    case "toolCancelled":
    case "toolError": {
      const { toolCallId } = payload.value
      return { ...activity, tools: activity.tools.filter((tool) => tool.toolCallId !== toolCallId) }
    }
    case "reasoningChunk":
      return payload.value.kind === ReasoningKind.RAW
        ? { ...activity, thinkingCharacters: activity.thinkingCharacters + payload.value.fragment.length }
        : activity
    case "textChunk":
      return activity.thinkingCharacters === 0 ? activity : { ...activity, thinkingCharacters: 0 }
    case "compactionStarted":
      return { ...activity, compacting: true }
    case "compactionFinished":
    case "compactionFailed":
      return { ...activity, compacting: false }
    default:
      return activity
  }
}

function reduceEvent(state: TimelineState, event: Event): TimelineState {
  const activity = state.agents.has(event.agentSessionId) ? state.activity : reduceActivity(state.activity, event)
  return reduceItems(activity === state.activity ? state : { ...state, activity }, event)
}

function reduceItems(state: TimelineState, event: Event): TimelineState {
  const { agentSessionId } = event
  const isRoot = !state.agents.has(agentSessionId)
  const payload = event.payload
  switch (payload.case) {
    case "inputAdmitted":
      return append(
        { ...state, pendingPlan: isRoot ? undefined : state.pendingPlan },
        { kind: "user", agentSessionId, inputId: payload.value.inputId, text: payload.value.content },
      )
    case "inputCanceled":
      return { ...state, items: state.items.filter((item) => item.kind !== "user" || item.inputId !== payload.value.inputId) }
    case "planCompleted":
      return append(
        { ...state, pendingPlan: isRoot && payload.value.dialog ? payload.value : state.pendingPlan },
        { kind: "plan", agentSessionId, markdown: payload.value.markdown, taskDeclarations: payload.value.taskDeclarations },
      )
    case "textChunk":
      return appendStreamed(state, event, "assistant", payload.value.fragment)
    case "reasoningChunk":
      // Raw reasoning is only a live "thinking" state, as in the terminal CLI; summaries stay in the transcript.
      return payload.value.kind === ReasoningKind.SUMMARY
        ? appendStreamed(state, event, "reasoning", payload.value.fragment, payload.value.completed)
        : state
    case "toolCallChunk": {
      const { toolCallId, toolName, argumentsFragment } = payload.value
      return updateTool(state, event, toolCallId, (tool) => ({ name: toolName || tool.name, args: tool.args + argumentsFragment }))
    }
    case "toolStarted":
      return updateTool(state, event, payload.value.toolCallId, () => ({ name: payload.value.toolName, status: "running" }))
    case "toolFinished":
      return updateTool(state, event, payload.value.toolCallId, () => ({ status: "finished", output: payload.value.result ?? "" }))
    case "toolCancelled":
      return updateTool(state, event, payload.value.toolCallId, () => ({ status: "cancelled" }))
    case "toolError":
      return updateTool(state, event, payload.value.toolCallId, () => ({ status: "error", output: payload.value.message }))
    case "toolRequestReceived":
      return payload.value.toolCallCount > 1 ? notice(state, event, `requested ${String(payload.value.toolCallCount)} tool calls`) : state
    case "agentStarted":
      if (!payload.value.parentAgentSessionId) return state
      return notice(
        {
          ...state,
          agents: new Map(state.agents).set(agentSessionId, {
            name: payload.value.name,
            parentAgentSessionId: payload.value.parentAgentSessionId,
          }),
        },
        event,
        `agent ${payload.value.name} started`,
        "active",
      )
    case "agentFinished":
      if (!payload.value.parentAgentSessionId) return state
      return notice(
        state,
        event,
        `agent ${payload.value.name} finished${payload.value.elapsedMs === undefined ? "" : ` (${formatDuration(Number(payload.value.elapsedMs))})`}`,
        "success",
      )
    case "agentFailed":
      if (!payload.value.parentAgentSessionId) return state
      return notice(state, event, `agent ${payload.value.name}: ${payload.value.message}`, "error")
    case "turnStarted":
      return isRoot
        ? { ...state, busy: true, pendingPlan: undefined, modelIcon: payload.value.modelAliasIcon }
        : state
    case "turnEnded": {
      if (!isRoot) return state
      const { finishReason, inputTokens, outputTokens } = payload.value
      return finishReason === "interrupted"
        ? notice(state, event, "agent interrupted", "error")
        : notice(state, event, `${finishReason} - ${String(inputTokens)} total in / ${String(outputTokens)} total out`, "success")
    }
    case "modeTurnCompleted":
      return isRoot ? { ...state, busy: false } : state
    case "turnFailed":
      return notice({ ...state, busy: state.busy && !isRoot }, event, payload.value.message, "error", payload.value.providerResponseBody)
    case "retryNotice":
      return notice(state, event, `retry ${String(payload.value.attempt)} in ${String(payload.value.retryAfterMs)} ms: ${payload.value.reason}`)
    case "compactionStarted":
      return notice(state, event, "compaction started", "active")
    case "compactionFinished":
      return notice(state, event, "compaction finished", "success")
    case "compactionFailed":
      return notice(state, event, `compaction failed: ${payload.value.message}`, "error")
    case "agentNotice":
      return payload.value.notice ? notice(state, event, `↻ ${payload.value.notice}`) : state
    case "skillLoaded":
      return isRoot ? notice(state, event, `↻ Skill loaded: ${payload.value.path}`) : state
    case "toolSnapshot":
      return reduceToolSnapshot(state, event, payload.value)
    case "sessionUsageSnapshot":
      // As the terminal CLI's RuntimeUsageTracker: an older snapshot never replaces a newer one.
      return state.usage && payload.value.revision < state.usage.revision ? state : { ...state, usage: payload.value }
    default:
      return state
  }
}

export function reduceTimeline(state: TimelineState, action: TimelineAction): TimelineState {
  switch (action.type) {
    case "event":
      return reduceEvent(state, action.event)
    case "print":
      return append(state, { kind: "print", lines: action.lines })
    case "error":
      return append(state, { kind: "error", message: action.message })
    case "planSettled":
      return { ...state, pendingPlan: undefined }
    case "connected":
      return { ...state, usage: undefined }
  }
}
