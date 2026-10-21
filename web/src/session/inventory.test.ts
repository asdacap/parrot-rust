import { describe, expect, it } from "vitest"

import { emptyInventory, observeInventory, type InventoryChunk, type OwnerInventory } from "@/session/inventory"

type Chunk = [instance: string, revision: number, index: number, final: boolean, items: string[], removed?: boolean]

function observe(chunks: Chunk[]): OwnerInventory<string> {
  return chunks.reduce<OwnerInventory<string>>(
    (owner, [instance, revision, index, final, items, removed = false]) =>
      observeInventory(owner, { instance, revision: BigInt(revision), index, final, removed, shape: "", items } satisfies InventoryChunk<string>),
    emptyInventory,
  )
}

describe("observeInventory", () => {
  it.each<[string, Chunk[], string[]]>([
    ["applies a single-chunk snapshot", [["a", 1, 0, true, ["one"]]], ["one"]],
    ["joins chunks in order", [["a", 1, 0, false, ["one"]], ["a", 1, 1, true, ["two"]]], ["one", "two"]],
    ["holds an incomplete snapshot back", [["a", 1, 0, true, ["one"]], ["a", 2, 0, false, ["two"]]], ["one"]],
    ["ignores an out-of-order chunk", [["a", 1, 0, false, ["one"]], ["a", 1, 2, true, ["three"]]], []],
    ["ignores an older revision", [["a", 2, 0, true, ["new"]], ["a", 1, 0, true, ["old"]]], ["new"]],
    ["keeps a removed inventory removed", [["a", 1, 0, true, ["one"], true], ["a", 2, 0, true, ["two"]]], []],
    ["replaces an instance for good", [["a", 5, 0, true, ["a"]], ["b", 1, 0, true, ["b"]], ["a", 6, 0, true, ["a again"]]], ["b"]],
  ])("%s", (_name, chunks, expected) => {
    expect(observe(chunks).items).toEqual(expected)
  })
})
