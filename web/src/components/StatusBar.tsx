import { Loader2Icon } from "lucide-react"

import { TurnModelAliasIconColor, type SessionUsageSnapshot, type TurnModelAliasIcon, type UserSession } from "@/gen/parrot_pb"
import { Button } from "@/components/ui/button"
import { formatDuration } from "@/lib/duration"
import { useElapsedSince } from "@/lib/useElapsedSince"
import { cn } from "@/lib/utils"
import type { Activity } from "@/session/timeline"
import { formatContext, formatCost, formatRate, formatTokenCount, formatTokens, type TokenRate } from "@/session/usageFormat"

const iconColors: Record<TurnModelAliasIconColor, string> = {
  [TurnModelAliasIconColor.BLACK]: "text-foreground",
  [TurnModelAliasIconColor.RED]: "text-red-500",
  [TurnModelAliasIconColor.GREEN]: "text-green-500",
  [TurnModelAliasIconColor.YELLOW]: "text-yellow-500",
  [TurnModelAliasIconColor.BLUE]: "text-blue-500",
  [TurnModelAliasIconColor.MAGENTA]: "text-fuchsia-500",
  [TurnModelAliasIconColor.CYAN]: "text-cyan-500",
  [TurnModelAliasIconColor.WHITE]: "text-foreground",
  [TurnModelAliasIconColor.GRAY]: "text-muted-foreground",
}

// The terminal CLI's modeline activity, most specific first.
function activityLabel(activity: Activity, busy: boolean): string {
  if (activity.waitingForFirstToken) return "Waiting for first token…"
  if (activity.requestAttempt > 0) {
    return activity.requestAttempt === 1 ? "Requesting…" : `Requesting (attempt ${String(activity.requestAttempt)})…`
  }
  const tool = activity.tools.at(-1)
  if (tool) return `Working: ${tool.name}`
  if (activity.compacting) return "Compacting…"
  // About four characters a token, as a rough live estimate.
  if (activity.thinkingCharacters > 0) return `Thinking (${formatTokenCount(Math.ceil(activity.thinkingCharacters / 4))} tokens)…`
  return busy ? "Working…" : ""
}

interface StatusBarProps {
  session: UserSession
  modelIcon: TurnModelAliasIcon | undefined
  activity: Activity
  busy: boolean
  usage: SessionUsageSnapshot | undefined
  tokenRate: TokenRate
  connected: boolean
  onCycleMode: () => void
}

export function StatusBar({ session, modelIcon, activity, busy, usage, tokenRate, connected, onCycleMode }: StatusBarProps) {
  // Time since the main turn started here, counted locally.
  const elapsed = useElapsedSince(busy, busy)
  const label = activityLabel(activity, busy)
  const context = usage ? formatContext(usage) : ""
  const contextLimit = Number(usage?.contextLimit ?? 0n)
  const contextShare = contextLimit > 0 ? Number(usage?.contextSize ?? 0n) / contextLimit : 0
  const details = [usage ? formatTokens(usage) : "", formatRate(tokenRate), usage ? formatCost(usage) : ""].filter(Boolean)

  return (
    <footer className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t px-4 py-1 font-mono text-xs text-muted-foreground">
      <Button variant="ghost" size="sm" title="Switch mode (Shift+Tab)" onClick={onCycleMode}>
        mode: {session.mode}
      </Button>
      {label && (
        <span className="flex items-center gap-1 text-foreground">
          <Loader2Icon className="size-3 animate-spin" />
          {label}
          {busy && ` (running ${formatDuration(elapsed)})`}
        </span>
      )}
      <span className="ml-auto flex flex-wrap items-center gap-x-2">
        {modelIcon && <span className={iconColors[modelIcon.color]}>{modelIcon.glyph}</span>}
        <span className="text-foreground">{session.model || "default model"}</span>
        {context && (
          <span className={cn(contextShare >= 0.9 ? "text-destructive" : contextShare >= 0.75 && "text-warning")}>({context})</span>
        )}
        {details.map((detail) => (
          <span key={detail}>· {detail}</span>
        ))}
        {!connected && <span className="text-destructive">· reconnecting…</span>}
      </span>
    </footer>
  )
}
