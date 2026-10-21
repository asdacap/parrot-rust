import { useEffect, useState } from "react"

export function useElapsedSince(resetKey: unknown, active: boolean) {
  const [elapsed, setElapsed] = useState(0)
  useEffect(() => {
    if (!active) return
    const started = Date.now()
    const tick = () => { setElapsed(Date.now() - started) }
    const reset = setTimeout(tick, 0)
    const timer = setInterval(tick, 1000)
    return () => {
      clearTimeout(reset)
      clearInterval(timer)
    }
  }, [resetKey, active])
  return elapsed
}
