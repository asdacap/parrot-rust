import { BotIcon } from "lucide-react"
import { memo, useEffect, useRef } from "react"

import { Markdown } from "@/components/Markdown"
import { TaskTree, fromDeclaration } from "@/components/TaskTree"
import { ToolEntry } from "@/components/ToolEntry"
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

const TimelineEntry = memo(function TimelineEntry({ item }: { item: TimelineItem }) {
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
})

// Consecutive items of one agent, shown under a single header.
interface AgentBlock {
  start: number
  agentSessionId: string | undefined
  items: TimelineItem[]
}

function groupConsecutiveByAgent(items: TimelineItem[]): AgentBlock[] {
  const blocks: AgentBlock[] = []
  items.forEach((item, index) => {
    const agentSessionId = "agentSessionId" in item ? item.agentSessionId : undefined
    const last = blocks.at(-1)
    if (last && last.agentSessionId === agentSessionId) last.items.push(item)
    else blocks.push({ start: index, agentSessionId, items: [item] })
  })
  return blocks
}

interface TimelineBlockProps {
  items: TimelineItem[]
  agent: AgentInfo | undefined
  depth: number
}

function TimelineBlock({ items, agent, depth }: TimelineBlockProps) {
  const entries = items.map((item, offset) => <TimelineEntry key={offset} item={item} />)
  if (!agent) return <div className="flex flex-col gap-2">{entries}</div>
  return (
    // Framed under its agent's name, nested as the terminal CLI indents a subagent's activity.
    <section className={cn("overflow-hidden rounded-md border", depthIndents[Math.min(depth, 4)])}>
      <header className="flex items-center gap-1.5 border-b bg-muted px-3 py-1.5 text-sm font-semibold">
        <BotIcon className="size-4 shrink-0" />
        {agent.name}
      </header>
      <div className="flex flex-col gap-2 px-3 py-2">{entries}</div>
    </section>
  )
}

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
        {groupConsecutiveByAgent(items).map((block) => (
          <TimelineBlock
            key={block.start}
            items={block.items}
            agent={block.agentSessionId === undefined ? undefined : agents.get(block.agentSessionId)}
            depth={block.agentSessionId === undefined ? 0 : agentDepth(agents, block.agentSessionId)}
          />
        ))}
      </div>
    </div>
  )
}
