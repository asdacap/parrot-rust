import { describe, expect, it } from "vitest"

import { formatDuration } from "@/lib/duration"

describe("formatDuration", () => {
  it.each([
    [0, "0s"],
    [1_499, "1s"],
    [65_000, "1m 05s"],
    [3_723_000, "1h 02m 03s"],
  ])("%i ms is %s", (milliseconds, expected) => {
    expect(formatDuration(milliseconds)).toBe(expected)
  })
})
