import { useEffect, useMemo, useState } from "react"

import { Badge } from "@/components/ui/badge"
import { formatDuration } from "@/lib/duration"
import { cn } from "@/lib/utils"
import type { ToolItem, ToolStatus } from "@/session/timeline"
import {
  isUnifiedDiff,
  parseToolArguments,
  questionArguments,
  tailLines,
  toolLabel,
  waitDurationMilliseconds,
} from "@/session/toolPresentation"

const toolBadgeVariants = {
  running: "secondary",
  finished: "outline",
  cancelled: "outline",
  error: "destructive",
} as const satisfies Record<ToolStatus, string>

// Counts from when the call first rendered, which is when it started for a live session.
function useElapsed(running: boolean) {
  const [started] = useState(() => Date.now())
  const [now, setNow] = useState(started)
  useEffect(() => {
    if (!running) return
    const timer = setInterval(() => { setNow(Date.now()) }, 1000)
    return () => { clearInterval(timer) }
  }, [running])
  return now - started
}

function DiffBlock({ diff }: { diff: string }) {
  return (
    <pre className="mt-2 max-h-80 overflow-auto font-mono whitespace-pre-wrap">
      {diff.split("\n").map((line, index) => (
        <div
          key={index}
          className={cn(
            line.startsWith("+") && !line.startsWith("+++") && "text-(--diff-added)",
            line.startsWith("-") && !line.startsWith("---") && "text-(--diff-removed)",
            line.startsWith("@@") && "text-muted-foreground",
          )}
        >
          {line}
        </div>
      ))}
    </pre>
  )
}

function ToolBody({ item, args }: { item: ToolItem; args: Record<string, unknown> }) {
  if (item.status === "error") return <pre className="mt-2 font-mono whitespace-pre-wrap text-destructive">{item.output}</pre>
  switch (item.name) {
    case "edit":
    case "write":
      return isUnifiedDiff(item.output) ? <DiffBlock diff={item.output} /> : <Output text={item.output} />
    case "exec_command":
      return <Output text={tailLines(item.output, 10)} full={item.output} />
    case "question":
      return (
        <ul className="mt-2 flex flex-col gap-1">
          {questionArguments(args).map((question, index) => (
            <li key={index}>
              {question.prompt}
              {question.options.map((option) => (
                <div key={option} className="pl-3 text-muted-foreground">- {option}</div>
              ))}
            </li>
          ))}
        </ul>
      )
    default:
      return (
        <>
          {item.args && <pre className="mt-2 overflow-x-auto font-mono whitespace-pre-wrap text-muted-foreground">{item.args}</pre>}
          <Output text={item.output} />
        </>
      )
  }
}

function Output({ text, full }: { text: string; full?: string }) {
  if (!text) return null
  return (
    <pre className="mt-2 max-h-80 overflow-auto font-mono whitespace-pre-wrap" title={full && full !== text ? "Last 10 lines" : undefined}>
      {text}
    </pre>
  )
}

export function ToolEntry({ item }: { item: ToolItem }) {
  const running = item.status === "running"
  const elapsed = useElapsed(running)
  const args = useMemo(() => parseToolArguments(item.args), [item.args])
  // The terminal CLI shows a wait only while it runs.
  if (item.name === "wait") {
    if (!running) return null
    const duration = waitDurationMilliseconds(args)
    return (
      <div className="text-xs text-muted-foreground">
        {toolLabel(item.name, args)}
        {duration !== undefined && ` · ${formatDuration(Math.max(0, duration - elapsed))} left`}
      </div>
    )
  }
  return (
    <div className="flex flex-col gap-1">
      <details className="rounded-md border px-3 py-1.5 text-xs" open={item.name === "question" || undefined}>
        <summary className="flex cursor-pointer items-center gap-2 select-none">
          <span className="min-w-0 flex-1 truncate font-mono">{toolLabel(item.name, args)}</span>
          {running && <span className="text-muted-foreground">running {formatDuration(elapsed)}</span>}
          <Badge variant={toolBadgeVariants[item.status]}>{item.status}</Badge>
        </summary>
        <ToolBody item={item} args={args} />
      </details>
    </div>
  )
}
