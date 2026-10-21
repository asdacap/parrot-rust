import { describe, expect, it } from "vitest"

import { retainedSamples, rollingRate } from "@/session/tokenRate"

describe("rollingRate", () => {
  it("averages the last 30 seconds of samples", () => {
    const samples = [
      { at: 0, input: 3_000, output: 300 },
      { at: 40_000, input: 60, output: 30 },
      { at: 45_000, input: 30, output: 0 },
    ]
    expect(rollingRate(retainedSamples(samples, 50_000))).toEqual({ input: 3, output: 1 })
  })
})
