import {
  AgentTaskProgressStatus,
  type AgentTaskProgressNode,
  type AgentTaskProgressSnapshot,
  type PlanTaskDeclaration,
} from "@/gen/parrot_pb"
import { AgentStatus, type AgentLine } from "@/components/AgentStatus"

// The terminal CLI's task icons (AgentTaskProgressFormatter).
const icons: Record<AgentTaskProgressStatus, string> = {
  [AgentTaskProgressStatus.UNSPECIFIED]: "?",
  [AgentTaskProgressStatus.PENDING]: "○",
  [AgentTaskProgressStatus.RUNNING]: "◐",
  [AgentTaskProgressStatus.SUCCEEDED]: "✓",
  [AgentTaskProgressStatus.FAILED]: "✗",
  [AgentTaskProgressStatus.CANCELED]: "■",
}

export interface TaskNode {
  name: string
  status: AgentTaskProgressStatus
  description: string
  detail: string
  hidden: boolean
  agentSessionId: string
  // The working agent's live line, standing in for the status icon.
  agent: AgentLine | undefined
  children: TaskNode[]
}

// A node's child agent's own task graph, when one is known, nests as its children.
export function fromProgress(
  node: AgentTaskProgressNode,
  trees: ReadonlyMap<string, AgentTaskProgressSnapshot>,
  ancestors: ReadonlySet<string> = new Set(),
): TaskNode {
  const nested = ancestors.has(node.agentSessionId) ? undefined : trees.get(node.agentSessionId)
  const inner = nested ? new Set([...ancestors, node.agentSessionId]) : ancestors
  return {
    name: node.name,
    status: node.status,
    description: node.description,
    detail: "",
    hidden: node.hidden,
    agentSessionId: node.agentSessionId,
    agent: undefined,
    children: (nested ? nested.rootNodes : node.children).map((child) => fromProgress(child, trees, inner)),
  }
}

// Settled tasks no longer change unless the owning agent sets them again.
export const isSettled = (node: TaskNode): boolean =>
  (node.status === AgentTaskProgressStatus.SUCCEEDED || node.status === AgentTaskProgressStatus.CANCELED) &&
  node.children.every(isSettled)

// Settled tasks keep their subtasks out of view.
export const showsChildren = (node: TaskNode): boolean =>
  node.status !== AgentTaskProgressStatus.SUCCEEDED && node.status !== AgentTaskProgressStatus.CANCELED

// As the terminal CLI's live task tree: a hidden task shows only while its agent works or a task below it shows,
// and a working agent's line stands in for its task's icon.
export function selectLive(nodes: TaskNode[], agentLines: ReadonlyMap<string, AgentLine>): TaskNode[] {
  const select = (candidates: TaskNode[], hiddenAncestor: boolean): TaskNode[] =>
    candidates.flatMap((node) => {
      const hidden = hiddenAncestor || node.hidden
      const children = select(node.children, hidden)
      const agent = agentLines.get(node.agentSessionId)
      return hidden && children.length === 0 && !agent ? [] : [{ ...node, agent, children }]
    })
  return select(nodes, false)
}

export const fromDeclaration = (task: PlanTaskDeclaration): TaskNode => ({
  name: task.name,
  status: task.status,
  description: task.description,
  detail: [
    task.dependencies.length > 0 ? `after ${task.dependencies.join(", ")}` : "",
    task.model ? `model ${task.model}` : "",
    task.acceptanceCriteria ? `done when ${task.acceptanceCriteria}` : "",
  ]
    .filter(Boolean)
    .join(" · "),
  hidden: task.hidden,
  agentSessionId: "",
  agent: undefined,
  children: task.payload.case === "children" ? task.payload.value.tasks.map(fromDeclaration) : [],
})

export function TaskTree({ title, nodes }: { title: string; nodes: TaskNode[] }) {
  return (
    <div className="font-mono text-xs">
      <div className="text-muted-foreground">{title}</div>
      <TaskList nodes={nodes} />
    </div>
  )
}

function TaskList({ nodes }: { nodes: TaskNode[] }) {
  return (
    <ul className="border-l pl-3">
      {nodes.map((node, index) => (
        <li key={index}>
          {node.agent ? <AgentStatus line={node.agent} /> : <span>{icons[node.status]} </span>}
          <span>{node.description || node.name}</span>
          {node.detail && <span className="text-muted-foreground"> · {node.detail}</span>}
          {showsChildren(node) && node.children.length > 0 && <TaskList nodes={node.children} />}
        </li>
      ))}
    </ul>
  )
}
