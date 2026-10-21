import { useSyncExternalStore } from "react"

// An empty hash opens the workspace's latest session, as the terminal CLI does; "#/" is the session list.
export type Route = { page: "default" } | { page: "sessions" } | { page: "session"; userSessionId: string }

function subscribe(onChange: () => void) {
  window.addEventListener("hashchange", onChange)
  return () => { window.removeEventListener("hashchange", onChange) }
}

export function useRoute(): Route {
  const hash = useSyncExternalStore(subscribe, () => location.hash)
  if (hash === "") return { page: "default" }
  const sessionMatch = /^#\/s\/(.+)$/.exec(hash)
  return sessionMatch?.[1] ? { page: "session", userSessionId: decodeURIComponent(sessionMatch[1]) } : { page: "sessions" }
}

export function sessionHash(userSessionId: string) {
  return `#/s/${encodeURIComponent(userSessionId)}`
}

export function navigateToSession(userSessionId: string, replace = false) {
  const hash = sessionHash(userSessionId)
  if (replace) location.replace(hash)
  else location.hash = hash
}
