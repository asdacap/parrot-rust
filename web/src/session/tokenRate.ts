import type { TokenRate } from "@/session/usageFormat"

// The terminal CLI's RollingTokenRateWindow: provider-call tokens over the last 30 seconds, per second.
const windowSeconds = 30

export interface TokenSample {
  at: number
  input: number
  output: number
}

export function retainedSamples(samples: TokenSample[], now: number): TokenSample[] {
  return samples.filter((sample) => sample.at > now - windowSeconds * 1000)
}

export function rollingRate(retained: TokenSample[]): TokenRate {
  return {
    input: retained.reduce((sum, sample) => sum + sample.input, 0) / windowSeconds,
    output: retained.reduce((sum, sample) => sum + sample.output, 0) / windowSeconds,
  }
}
