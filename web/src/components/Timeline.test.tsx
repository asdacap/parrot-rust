import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"

import { Timeline } from "@/components/Timeline"
import type { AgentInfo, TimelineItem } from "@/session/timeline"

describe("Timeline", () => {
  it("shows one badge per run of consecutive items from the same subagent", () => {
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

    const badges = [...renderToStaticMarkup(<Timeline items={items} agents={agents} />).matchAll(/data-slot="badge"[^>]*>([^<]*)</g)]

    expect(badges.map(([, name]) => name)).toEqual(["explorer", "reviewer", "explorer"])
  })
})
