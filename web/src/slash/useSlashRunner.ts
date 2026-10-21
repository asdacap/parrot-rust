import { ConnectError } from "@connectrpc/connect"
import { useEffect, useState, type Dispatch } from "react"

import type { UserSession } from "@/gen/parrot_pb"
import type { AnswerSlashPromptRequest, SlashFrame } from "@/gen/web_pb"
import { navigateToSession } from "@/lib/router"
import { parrotWeb } from "@/rpc/client"
import type { TimelineAction } from "@/session/timeline"

export type SlashPromptFrame = Extract<SlashFrame["payload"], { case: "select" | "readText" | "confirm" | "show" }>
export type SlashAnswer = AnswerSlashPromptRequest["answer"]

// The server runs a command immediately, so commands submitted during a turn wait here until the session is idle.
export function useSlashRunner(
  userSessionId: string,
  busy: boolean,
  dispatch: Dispatch<TimelineAction>,
  onSessionChanged: (session: UserSession) => void,
) {
  const [queued, setQueued] = useState<string[]>([])
  const [running, setRunning] = useState<{ text: string } | null>(null)
  const [prompt, setPrompt] = useState<{ runId: string; frame: SlashPromptFrame } | null>(null)
  const [loadingActivity, setLoadingActivity] = useState<string | null>(null)

  const [next, ...rest] = queued
  if (!busy && running === null && next !== undefined) {
    setQueued(rest)
    setRunning({ text: next })
  }

  useEffect(() => {
    if (running === null) return
    const abort = new AbortController()
    void (async () => {
      let runId = ""
      let replacementSessionId: string | null = null
      try {
        for await (const frame of parrotWeb.runSlashCommand({ userSessionId, text: running.text }, { signal: abort.signal })) {
          const payload = frame.payload
          switch (payload.case) {
            case "started":
              runId = payload.value.runId
              break
            case "select":
            case "readText":
            case "confirm":
            case "show":
              setPrompt({ runId, frame: payload })
              break
            case "print":
              dispatch({ type: "print", lines: payload.value.lines })
              break
            case "error":
              dispatch({ type: "error", message: payload.value.message })
              break
            case "loading":
              setLoadingActivity(payload.value.activity)
              break
            case "loaded":
              setLoadingActivity(null)
              break
            case "sessionReplaced":
              replacementSessionId = payload.value.id
              break
            case "sessionUpdated":
              onSessionChanged(payload.value)
              break
          }
        }
      } catch (error) {
        if (!abort.signal.aborted) dispatch({ type: "error", message: ConnectError.from(error).message })
      }
      if (abort.signal.aborted) return
      setPrompt(null)
      setLoadingActivity(null)
      setRunning(null)
      if (replacementSessionId) navigateToSession(replacementSessionId)
    })()
    return () => { abort.abort() }
  }, [running, userSessionId, dispatch, onSessionChanged])

  function submit(text: string) {
    setQueued((current) => [...current, text])
  }

  async function answer(answer: SlashAnswer) {
    if (!prompt) return
    setPrompt(null)
    try {
      await parrotWeb.answerSlashPrompt({ runId: prompt.runId, promptId: prompt.frame.value.promptId, answer })
    } catch (error) {
      dispatch({ type: "error", message: ConnectError.from(error).message })
    }
  }

  return { queued, prompt, loadingActivity, submit, answer }
}
