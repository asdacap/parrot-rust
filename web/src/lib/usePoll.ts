import { useEffect } from "react"

const pollMilliseconds = 250

export function usePoll(poll: () => Promise<unknown>, pollImmediately: boolean) {
  useEffect(() => {
    let polling = false
    const run = () => {
      if (polling) return
      polling = true
      poll()
        .catch(() => undefined)
        .finally(() => { polling = false })
    }
    if (pollImmediately) run()
    const timer = setInterval(run, pollMilliseconds)
    return () => { clearInterval(timer) }
  }, [poll, pollImmediately])
}
