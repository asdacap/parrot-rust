import { create } from "@bufbuild/protobuf"
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"

import {
  AgentTaskProgressNodeSchema,
  AgentTaskProgressSnapshotSchema,
  AgentTaskProgressStatus,
} from "@/gen/parrot_pb"
import { TaskTree, fromProgress, isSettled } from "@/components/TaskTree"

describe("TaskTree", () => {
  it.each([
    AgentTaskProgressStatus.UNSPECIFIED,
    AgentTaskProgressStatus.PENDING,
    AgentTaskProgressStatus.RUNNING,
    AgentTaskProgressStatus.SUCCEEDED,
    AgentTaskProgressStatus.FAILED,
    AgentTaskProgressStatus.CANCELED,
  ])("hides descendants only when the parent's status is succeeded or canceled (%s)", (status) => {
    for (const separateChildGraph of [false, true]) {
      const descendants = [
        create(AgentTaskProgressNodeSchema, { name: "running descendant", status: AgentTaskProgressStatus.RUNNING }),
        create(AgentTaskProgressNodeSchema, { name: "pending descendant", status: AgentTaskProgressStatus.PENDING }),
      ]
      const parent = create(AgentTaskProgressNodeSchema, {
        name: "parent task",
        status,
        agentSessionId: separateChildGraph ? "parent-agent" : "",
        children: separateChildGraph ? [] : descendants,
      })
      const trees = new Map([
        ["parent-agent", create(AgentTaskProgressSnapshotSchema, { rootNodes: descendants })],
      ])
      const before = structuredClone({ parent, trees })
      const node = fromProgress(parent, trees)
      const markup = renderToStaticMarkup(<TaskTree title="Agent tasks:" nodes={[node]} />)
      const hideChildren = status === AgentTaskProgressStatus.SUCCEEDED || status === AgentTaskProgressStatus.CANCELED

      expect(markup).toContain("parent task")
      expect(markup.includes("running descendant")).toBe(!hideChildren)
      expect(markup.includes("pending descendant")).toBe(!hideChildren)
      expect(node.children).toHaveLength(2)
      expect(isSettled(node)).toBe(false)
      expect({ parent, trees }).toEqual(before)
    }
  })
})
