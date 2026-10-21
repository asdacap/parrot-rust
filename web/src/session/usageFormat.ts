import type { SessionUsageSnapshot } from "@/gen/parrot_pb"

// The terminal CLI's modeline formatting (RuntimeUsage.cs), so both read the same.
function scaled(value: number, fractionDigits: number) {
  const format = (scaledValue: number, suffix: string) =>
    `${String(Number(scaledValue.toFixed(fractionDigits)))}${suffix}`
  if (Math.abs(value) >= 1_000_000) return format(value / 1_000_000, "M")
  if (Math.abs(value) >= 1_000) return format(value / 1_000, "k")
  return format(value, "")
}

export const formatTokenCount = (count: number) => scaled(count, 1)

export function formatTokens(usage: SessionUsageSnapshot): string {
  const input = Number(usage.inputTokens)
  const output = Number(usage.outputTokens)
  if (input === 0 && output === 0) return ""
  const cached = Number(usage.cachedInputTokens)
  const tokens = `+${formatTokenCount(input)}i +${formatTokenCount(output)}o`
  return cached > 0 && input > 0 ? `${tokens} (+${((cached / input) * 100).toFixed(2)}% cache)` : tokens
}

export function formatContext(usage: SessionUsageSnapshot): string {
  const size = Number(usage.contextSize)
  const limit = Number(usage.contextLimit)
  if (limit > 0) return `${formatTokenCount(size)}/${formatTokenCount(limit)}`
  return size > 0 ? formatTokenCount(size) : ""
}

export function formatCost(usage: SessionUsageSnapshot): string {
  const cost = usage.inputCost + usage.outputCost
  if (cost <= 0) return ""
  return `$${cost.toFixed(cost < 0.01 ? 4 : 2)}`
}

export interface TokenRate {
  input: number
  output: number
}

const formatTokenRate = (rate: number) => scaled(rate, Math.abs(rate) >= 1_000 ? 1 : 2)

export const formatRate = (rate: TokenRate) =>
  rate.input > 0 || rate.output > 0 ? `${formatTokenRate(rate.input)}i/s ${formatTokenRate(rate.output)}o/s` : ""
