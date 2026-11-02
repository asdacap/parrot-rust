import { Loader2Icon } from "lucide-react"
import { useMemo } from "react"

import { ShellProcessActivityKind, type AgentTaskProgressSnapshot } from "@/gen/parrot_pb"
import type { ActiveShellProcess } from "@/gen/process_pb"
import type { QueueState } from "@/gen/queue_pb"
import { AgentStatus, type AgentLine } from "@/components/AgentStatus"
import { activityLabel } from "@/components/StatusBar"
import { TaskTree, fromProgress, isSettled, selectLive, showsChildren, type TaskNode } from "@/components/TaskTree"
import { formatDuration } from "@/lib/duration"
import { useElapsedSince } from "@/lib/useElapsedSince"
import { cn } from "@/lib/utils"
import type { OwnerInventory } from "@/session/inventory"
import { agentDepth, type Activity, type AgentActivity, type AgentInfo } from "@/session/timeline"
import { formatContext, formatTokens } from "@/session/usageFormat"

// Deeper agents stop indenting further, so the live area keeps its width.
const depthIndents = ["", "ml-4", "ml-8", "ml-12", "ml-16"] as const

type LiveContent =
  | { kind: "tasks"; nodes: TaskNode[] }
  | { kind: "agent"; line: AgentLine }
  | { kind: "idleAgent"; name: string }
  | { kind: "process"; process: ActiveShellProcess }
  | { kind: "queue"; queue: QueueState }

// One row of the live agent tree, placed under the agent session that owns it.
interface LiveRow {
  owner: string
  rank: number
  id: string
  depth: number
  label: string
  content: LiveContent
}

interface LiveActivityProps {
  agents: ReadonlyMap<string, AgentInfo>
  agentActivity: ReadonlyMap<string, AgentActivity>
  // The main agent's activity; a process stays out of view while the tool that started it runs.
  activity: Activity
  queues: ReadonlyMap<string, OwnerInventory<QueueState>>
  processes: ReadonlyMap<string, OwnerInventory<ActiveShellProcess>>
  // The newest task graph per agent session.
  taskProgress: ReadonlyMap<string, AgentTaskProgressSnapshot>
}

function isDescendant(agents: ReadonlyMap<string, AgentInfo>, agentSessionId: string, ancestor: string): boolean {
  let current = agents.get(agentSessionId)
  for (let steps = 0; current && steps < agents.size; steps++) {
    if (current.parentAgentSessionId === ancestor) return true
    current = agents.get(current.parentAgentSessionId)
  }
  return false
}

// The terminal CLI's live order: every agent session after its descendants, siblings by id, so the main agent comes last.
function orderAgents(agents: ReadonlyMap<string, AgentInfo>): Map<string, number> {
  const children = new Map<string, string[]>()
  for (const [agentSessionId, { parentAgentSessionId }] of agents) {
    children.set(parentAgentSessionId, [...(children.get(parentAgentSessionId) ?? []), agentSessionId])
  }
  const seen = new Set<string>()
  const ordered: string[] = []
  const visit = (agentSessionId: string): void => {
    if (seen.has(agentSessionId)) return
    seen.add(agentSessionId)
    for (const child of (children.get(agentSessionId) ?? []).sort()) visit(child)
    ordered.push(agentSessionId)
  }
  const mainAgents = [...children.keys()].filter((agentSessionId) => !agents.has(agentSessionId)).sort()
  for (const agentSessionId of [...mainAgents, ...[...agents.keys()].sort()]) visit(agentSessionId)
  return new Map(ordered.map((agentSessionId, index) => [agentSessionId, index]))
}

const flattenShown = (nodes: TaskNode[]): TaskNode[] =>
  nodes.flatMap((node) => [node, ...(showsChildren(node) ? flattenShown(node.children) : [])])

// The terminal CLI's agent spinner text: context, tokens, then what the agent is doing.
function formatAgentStatus({ activity, statistics }: AgentActivity): string {
  const context = statistics ? formatContext(statistics) : ""
  return [context && `(${context})`, statistics ? formatTokens(statistics) : "", activityLabel(activity, false)].filter(Boolean).join(" ")
}

// The rows of the terminal CLI's live area, ordered by owner, then rank, then id.
function placeRows({ agents, agentActivity, activity, queues, processes, taskProgress }: LiveActivityProps): LiveRow[] {
  const nameOf = (owner: string) => agents.get(owner)?.name ?? ""
  const agentLines = new Map(
    [...agentActivity]
      .filter(([, live]) => live.inTurn)
      .map(([owner, live]): [string, AgentLine] => [owner, { label: nameOf(owner), icon: live.modelIcon, text: formatAgentStatus(live) }]),
  )
  const working = [...agentLines.keys()]
  // A child agent's graph nests under its task in its owner's graph instead of standing alone.
  const nested = new Set([...taskProgress.values()].flatMap((tree) => tree.rootNodes.map((node) => node.agentSessionId)))
  const trees = [...taskProgress]
    .filter(([owner]) => !nested.has(owner))
    .map(([owner, tree]) => ({ owner, nodes: tree.rootNodes.map((node) => fromProgress(node, taskProgress)) }))
    // A settled graph stays while an agent below its owner still works.
    .filter(({ owner, nodes }) => !nodes.every(isSettled) || working.some((agentSessionId) => isDescendant(agents, agentSessionId, owner)))
    .map(({ owner, nodes }) => ({ owner, nodes: selectLive(nodes, agentLines) }))
    .filter(({ nodes }) => nodes.length > 0)
  const shown = trees.flatMap(({ nodes }) => flattenShown(nodes))
  const embedded = new Set(shown.filter((node) => node.agent).map((node) => node.agentSessionId))
  const nestedOwners = shown
    .filter((node) => showsChildren(node) && node.children.length > 0 && taskProgress.has(node.agentSessionId))
    .map((node) => node.agentSessionId)
  const toolsOf = (owner: string) => (agents.has(owner) ? agentActivity.get(owner)?.activity : activity)?.tools ?? []
  const running = [...processes.values()]
    .flatMap((owner) => owner.items)
    .filter((process) => !toolsOf(process.ownerAgentSessionId).some((tool) => tool.toolCallId === process.originToolCallId))
  const waiting = [...queues.values()].flatMap((owner) => owner.items).filter((queue) => queue.itemCount > 0)
  const owners = new Set([
    ...working,
    ...running.map((process) => process.ownerAgentSessionId),
    ...waiting.map((queue) => queue.ownerAgentSessionId),
    ...trees.map((tree) => tree.owner),
    ...nestedOwners,
  ])
  owners.delete("")
  // An idle agent stays as the head of its own rows and its descendants'.
  const idle = [...agents.keys()].filter(
    (agentSessionId) =>
      !agentLines.has(agentSessionId) &&
      [...owners].some((owner) => owner === agentSessionId || isDescendant(agents, owner, agentSessionId)),
  )

  const place = (owner: string, rank: number, id: string, content: LiveContent): LiveRow => ({
    owner,
    rank,
    id,
    depth: agentDepth(agents, owner),
    label: nameOf(owner),
    content,
  })
  const rows = [
    ...trees.map(({ owner, nodes }) => place(owner, 0, "task", { kind: "tasks", nodes })),
    ...running.map((process) => ({
      ...place(process.ownerAgentSessionId, 1, process.processId, { kind: "process", process }),
      depth: process.depth,
      label: process.depth > 0 ? process.ownerAgentName || process.ownerAgentSessionId : "",
    })),
    ...waiting.map((queue) => place(queue.ownerAgentSessionId, 2, queue.name, { kind: "queue", queue })),
    ...[...agentLines]
      .filter(([owner]) => !embedded.has(owner))
      .map(([owner, line]) => place(owner, 3, "agent", { kind: "agent", line })),
    ...idle.map((owner) => ({
      ...place(owner, 3, "agent", { kind: "idleAgent", name: nameOf(owner) }),
      depth: agentDepth(agents, owner) - 1,
    })),
  ]
  const order = orderAgents(agents)
  const position = (owner: string) => (owner ? (order.get(owner) ?? order.size) : -1)
  return rows.sort(
    (left, right) =>
      position(left.owner) - position(right.owner) ||
      left.rank - right.rank ||
      Number(left.id > right.id) - Number(left.id < right.id),
  )
}

function LiveRowContent({ row, sinceSnapshot }: { row: LiveRow; sinceSnapshot: number }) {
  const label = row.label && `[${row.label}] `
  const { content } = row
  switch (content.kind) {
    case "tasks":
      return <TaskTree title={`${label}Agent tasks:`} nodes={content.nodes} />
    case "agent":
      return <AgentStatus line={content.line} />
    case "idleAgent":
      return <>♟ {label}agent {content.name}</>
    case "process": {
      const { process } = content
      return (
        <span className="flex items-center gap-1">
          {process.activityKind === ShellProcessActivityKind.MONITOR ? "◉" : <Loader2Icon className="size-3 animate-spin" />}
          {label}$ {process.command} ({process.name} running {formatDuration(Number(process.elapsedMs) + sinceSnapshot)})
        </span>
      )
    }
    case "queue": {
      const { queue } = content
      return (
        <>
          {label}queue: {queue.name} · {queue.itemCount} {queue.itemCount === 1 ? "item" : "items"}
          {queue.description && ` — ${queue.description}`}
        </>
      )
    }
  }
}

// The terminal CLI's live area: task graphs, working agents, queues and background processes, each under its agent.
export function LiveActivity({ agents, agentActivity, activity, queues, processes, taskProgress }: LiveActivityProps) {
  const rows = useMemo(
    () => placeRows({ agents, agentActivity, activity, queues, processes, taskProgress }),
    [agents, agentActivity, activity, queues, processes, taskProgress],
  )
  // Milliseconds since the latest process snapshot, which each elapsed time is as of.
  const sinceSnapshot = useElapsedSince(processes, rows.some((row) => row.content.kind === "process"))

  if (rows.length === 0) return null
  return (
    <ul className="mx-auto flex w-full max-w-4xl flex-col gap-0.5 px-4 pb-2 font-mono text-xs text-muted-foreground">
      {rows.map((row) => (
        <li key={`${row.owner}/${String(row.rank)}/${row.id}`} className={cn(depthIndents[Math.min(row.depth, 4)])}>
          <LiveRowContent row={row} sinceSnapshot={sinceSnapshot} />
        </li>
      ))}
    </ul>
  )
}
