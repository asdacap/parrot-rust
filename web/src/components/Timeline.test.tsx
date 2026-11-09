import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"

import { Timeline } from "@/components/Timeline"
import type { AgentInfo, TimelineItem } from "@/session/timeline"

describe("Timeline", () => {
  it("shows one agent header per run of consecutive items from the same subagent", () => {
    const agents = new Map<string, AgentInfo>([
      ["explorer", { name: "explorer", parentAgentSessionId: "root" }],
      ["reviewer", { name: "reviewer", parentAgentSessionId: "root" }],
    ])
    const items: TimelineItem[] = [
      { kind: "assistant", agentSessionId: "root", text: "root first" },
      { kind: "assistant", agentSessionId: "explorer", text: "explorer first" },
      { kind: "notice", agentSessionId: "explorer", tone: "muted", text: "explorer second", detail: "" },
      { kind: "assistant", agentSessionId: "explorer", text: "explorer third" },
      { kind: "assistant", agentSessionId: "reviewer", text: "reviewer first" },
      { kind: "assistant", agentSessionId: "root", text: "root second" },
      { kind: "assistant", agentSessionId: "explorer", text: "explorer fourth" },
    ]

    const headers = [...renderToStaticMarkup(<Timeline items={items} agents={agents} />).matchAll(/<header\b[^>]*>([\s\S]*?)<\/header>/g)]
    const names = headers.map(([, content = ""]) => content.replace(/<[^>]*>/g, "").trim())

    expect(names).toEqual(["explorer", "reviewer", "explorer"])
  })
})
