import { describe, expect, it } from "vitest"

import { isUnifiedDiff, parseToolArguments, tailLines, toolLabel } from "@/session/toolPresentation"

describe("toolLabel", () => {
  it.each([
    ["edit", '{"path":"src/a.ts"}', "edit src/a.ts"],
    ["read", '{"path":"README.md"}', "read README.md"],
    ["glob", '{"pattern":"**/*.cs","path":"src"}', 'glob "**/*.cs" in src'],
    ["exec_command", '{"cmd":"ls -la"}', "$ ls -la"],
    ["web_fetch", '{"url":"https://example.com"}', "GET https://example.com"],
    ["question", '{"questions":[{"prompt":"a"},{"prompt":"b"}]}', "Question · 2 items"],
    ["wait", '{"duration_ms":1000}', "Wait for incoming activity"],
    ["status", "{", "status"],
  ])("%s %s", (name, args, expected) => {
    expect(toolLabel(name, parseToolArguments(args))).toBe(expected)
  })
})

describe("output helpers", () => {
  it("recognises a unified diff and keeps the last lines", () => {
    expect([isUnifiedDiff("--- a/x\n+++ b/x\n@@"), isUnifiedDiff("patched"), tailLines("1\n2\n3\n", 2)]).toEqual([true, false, "2\n3"])
  })
})
