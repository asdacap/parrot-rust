import { Loader2Icon } from "lucide-react"

import type { TurnModelAliasIcon } from "@/gen/parrot_pb"
import { iconColors } from "@/components/StatusBar"

// A working subagent's line in the terminal CLI's live area: its name, model icon and spinner text.
export interface AgentLine {
  label: string
  icon: TurnModelAliasIcon | undefined
  text: string
}

export function AgentStatus({ line }: { line: AgentLine }) {
  return (
    <span>
      <Loader2Icon className="inline size-3 animate-spin" /> [{line.label}]{" "}
      {line.icon && <span className={iconColors[line.icon.color]}>{line.icon.glyph} </span>}
      {line.text}{" "}
    </span>
  )
}
