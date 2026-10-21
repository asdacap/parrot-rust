import { create } from "@bufbuild/protobuf"
import { describe, expect, it } from "vitest"

import { SessionUsageSnapshotSchema } from "@/gen/parrot_pb"
import { formatContext, formatCost, formatRate, formatTokens } from "@/session/usageFormat"

describe("usage formatting", () => {
  it.each([
    [{}, ["", "", ""]],
    [
      { inputTokens: 12_345n, cachedInputTokens: 6_000n, outputTokens: 800n, contextSize: 42_000n, contextLimit: 200_000n, inputCost: 0.004 },
      ["+12.3ki +800o (+48.60% cache)", "42k/200k", "$0.0040"],
    ],
    [{ inputTokens: 1n, outputTokens: 2_500_000n, contextSize: 900n, inputCost: 1.5, outputCost: 0.25 }, ["+1i +2.5Mo", "900", "$1.75"]],
  ])("%o", (usage, expected) => {
    const snapshot = create(SessionUsageSnapshotSchema, usage)
    expect([formatTokens(snapshot), formatContext(snapshot), formatCost(snapshot)]).toEqual(expected)
  })

  it("formats token rates like the terminal CLI", () => {
    expect([formatRate({ input: 0, output: 0 }), formatRate({ input: 12.345, output: 1_540 })]).toEqual(["", "12.35i/s 1.5ko/s"])
  })
})
