import { Code, ConnectError } from "@connectrpc/connect"
import { useEffect, useReducer, useState } from "react"

import { delay } from "@/lib/delay"
import { parrot } from "@/rpc/client"
import { emptyTimeline, reduceTimeline } from "@/session/timeline"
import { retainedSamples, rollingRate, type TokenSample } from "@/session/tokenRate"
import type { TokenRate } from "@/session/usageFormat"

const initialRetryMilliseconds = 500
const maximumRetryMilliseconds = 10_000

export function useSessionEvents(userSessionId: string) {
  const [timeline, dispatch] = useReducer(reduceTimeline, emptyTimeline)
  const [connected, setConnected] = useState(false)
  // Set when the session is no longer hosted anywhere this server can reach; retrying cannot help.
  const [lost, setLost] = useState("")
  const [tokenRate, setTokenRate] = useState<TokenRate>({ input: 0, output: 0 })

  useEffect(() => {
    const abort = new AbortController()
    // The first connection replays the whole transcript; a reconnection replays only what came after the last event seen.
    const seenEventIds = new Set<string>()
    let lastEventId = ""
    // Provider-call usage is transient, so it is sampled as it arrives rather than replayed.
    let samples: TokenSample[] = []
    const rateTimer = setInterval(() => {
      if (samples.length === 0) return
      samples = retainedSamples(samples, Date.now())
      const next = rollingRate(samples)
      setTokenRate((previous) => (previous.input === next.input && previous.output === next.output ? previous : next))
    }, 1000)
    void (async () => {
      let retryMilliseconds = initialRetryMilliseconds
      while (!abort.signal.aborted) {
        try {
          const stream = parrot.listen({ userSessionId, replay: true, replayAfterEventId: lastEventId }, { signal: abort.signal })
          dispatch({ type: "connected" })
          for await (const event of stream) {
            setConnected(true)
            retryMilliseconds = initialRetryMilliseconds
            if (event.id) {
              if (seenEventIds.has(event.id)) continue
              seenEventIds.add(event.id)
              lastEventId = event.id
            }
            if (event.payload.case === "providerCallUsage") {
              samples.push({ at: Date.now(), input: Number(event.payload.value.inputTokens), output: Number(event.payload.value.outputTokens) })
            }
            dispatch({ type: "event", event })
          }
        } catch (error) {
          const failure = ConnectError.from(error)
          if (failure.code === Code.NotFound) {
            setLost(failure.message)
            return
          }
          // Otherwise reconnected below.
        }
        setConnected(false)
        await delay(retryMilliseconds, abort.signal)
        retryMilliseconds = Math.min(retryMilliseconds * 2, maximumRetryMilliseconds)
      }
    })()
    return () => {
      abort.abort()
      clearInterval(rateTimer)
    }
  }, [userSessionId])

  return { timeline, dispatch, connected, lost, tokenRate }
}
