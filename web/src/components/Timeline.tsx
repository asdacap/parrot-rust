import { memo, useEffect, useRef } from "react"

import { Markdown } from "@/components/Markdown"
import { TaskTree, fromDeclaration } from "@/components/TaskTree"
import { ToolEntry } from "@/components/ToolEntry"
import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"
import { agentDepth, type AgentInfo, type NoticeTone, type TimelineItem } from "@/session/timeline"

// Deeper subagents stop indenting further, so the transcript keeps its width.
const depthIndents = ["", "", "ml-4", "ml-8", "ml-12"] as const

const noticeTones = {
  muted: "text-muted-foreground",
  active: "text-muted-foreground",
  success: "text-(--diff-added)",
  error: "text-destructive",
} as const satisfies Record<NoticeTone, string>

function TimelineEntry({ item }: { item: TimelineItem }) {
  switch (item.kind) {
    case "user":
      return <div className="self-end rounded-lg bg-secondary px-3 py-2 text-sm whitespace-pre-wrap">{item.text}</div>
    case "assistant":
      return <div className="text-sm"><Markdown text={item.text} /></div>
    case "reasoning":
      return (
        <details className="text-xs text-muted-foreground">
          <summary className="cursor-pointer select-none">Reasoning</summary>
          <div className="mt-1"><Markdown text={item.text} /></div>
        </details>
      )
    case "tool":
      return <ToolEntry item={item} />
    case "notice":
      return item.detail ? (
        <details className={cn("text-xs", noticeTones[item.tone])}>
          <summary className="cursor-pointer select-none">{item.text}</summary>
          <pre className="mt-1 overflow-x-auto font-mono whitespace-pre-wrap">{item.detail}</pre>
        </details>
      ) : (
        <div className={cn("text-xs", noticeTones[item.tone])}>{item.text}</div>
      )
    case "plan":
      return (
        <div className="flex flex-col gap-2 rounded-md border bg-card px-3 py-2 text-sm">
          <div className="text-xs font-medium text-muted-foreground uppercase">Plan</div>
          {item.markdown && <Markdown text={item.markdown} />}
          {item.taskDeclarations.length > 0 && <TaskTree title="Tasks:" nodes={item.taskDeclarations.map(fromDeclaration)} />}
        </div>
      )
    case "print":
      return <pre className="font-mono text-xs whitespace-pre-wrap">{item.lines.join("\n")}</pre>
    case "error":
      return <div className="text-xs text-destructive">{item.message}</div>
  }
}

interface TimelineRowProps {
  item: TimelineItem
  agent: AgentInfo | undefined
  depth: number
}

const TimelineRow = memo(function TimelineRow({ item, agent, depth }: TimelineRowProps) {
  return (
    // Nested under its agent, as the terminal CLI indents a subagent's activity.
    <div className={cn("flex flex-col gap-1", depth > 0 && "border-l pl-3", depthIndents[Math.min(depth, 4)])}>
      {agent && <Badge variant="outline">{agent.name}</Badge>}
      <TimelineEntry item={item} />
    </div>
  )
})

interface TimelineProps {
  items: TimelineItem[]
  agents: ReadonlyMap<string, AgentInfo>
}

export function Timeline({ items, agents }: TimelineProps) {
  const scrollerRef = useRef<HTMLDivElement>(null)
  const stickToBottom = useRef(true)

  useEffect(() => {
    const scroller = scrollerRef.current
    if (scroller && stickToBottom.current) scroller.scrollTop = scroller.scrollHeight
  }, [items])

  return (
    <div
      ref={scrollerRef}
      className="min-h-0 flex-1 overflow-y-auto"
      onScroll={(event) => {
        const scroller = event.currentTarget
        stickToBottom.current = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 48
      }}
    >
      <div className="mx-auto flex max-w-4xl flex-col gap-2 px-4 py-4">
        {items.map((item, index) => {
          const agentSessionId = "agentSessionId" in item ? item.agentSessionId : undefined
          return (
            <TimelineRow
              key={index}
              item={item}
              agent={agentSessionId === undefined ? undefined : agents.get(agentSessionId)}
              depth={agentSessionId === undefined ? 0 : agentDepth(agents, agentSessionId)}
            />
          )
        })}
      </div>
    </div>
  )
}
