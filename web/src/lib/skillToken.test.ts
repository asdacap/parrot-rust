import { describe, expect, it } from "vitest"

import { findSkillToken } from "@/lib/skillToken"

describe("findSkillToken", () => {
  it.each([
    ["use $rev", 8, { start: 4, end: 8, prefix: "rev" }],
    ["use $review now", 7, { start: 4, end: 11, prefix: "re" }],
    ["$", 1, { start: 0, end: 1, prefix: "" }],
    ["a$rev", 5, undefined],
    ["echo $HOME", 10, undefined],
    ["no token", 8, undefined],
  ])("%s at %i", (text, caret, expected) => {
    expect(findSkillToken(text, caret)).toEqual(expected)
  })
})
