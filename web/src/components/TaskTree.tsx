import {
  AgentTaskProgressStatus,
  type AgentTaskProgressNode,
  type AgentTaskProgressSnapshot,
  type PlanTaskDeclaration,
} from "@/gen/parrot_pb"

// The terminal CLI's task icons (AgentTaskProgressFormatter).
const icons: Record<AgentTaskProgressStatus, string> = {
  [AgentTaskProgressStatus.UNSPECIFIED]: "?",
  [AgentTaskProgressStatus.PENDING]: "○",
  [AgentTaskProgressStatus.RUNNING]: "◐",
  [AgentTaskProgressStatus.SUCCEEDED]: "✓",
  [AgentTaskProgressStatus.FAILED]: "✗",
  [AgentTaskProgressStatus.CANCELED]: "■",
}

interface TaskNode {
  name: string
  status: AgentTaskProgressStatus
  description: string
  detail: string
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
    children: (nested ? nested.rootNodes : node.children).map((child) => fromProgress(child, trees, inner)),
  }
}

// Settled tasks no longer change unless the owning agent sets them again.
export const isSettled = (node: TaskNode): boolean =>
  (node.status === AgentTaskProgressStatus.SUCCEEDED || node.status === AgentTaskProgressStatus.CANCELED) &&
  node.children.every(isSettled)

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
          <span>{icons[node.status]} </span>
          <span>{node.description || node.name}</span>
          {node.detail && <span className="text-muted-foreground"> · {node.detail}</span>}
          {node.children.length > 0 && <TaskList nodes={node.children} />}
        </li>
      ))}
    </ul>
  )
}
