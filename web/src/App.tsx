import { ConnectError } from "@connectrpc/connect"
import { useEffect, useState } from "react"

import { SessionPicker } from "@/components/SessionPicker"
import { SessionView } from "@/components/SessionView"
import { navigateToSession, useRoute } from "@/lib/router"
import { parrotWeb } from "@/rpc/client"

export function App() {
  const route = useRoute()
  switch (route.page) {
    case "default":
      return <DefaultSession />
    case "session":
      return <SessionView key={route.userSessionId} userSessionId={route.userSessionId} />
    case "sessions":
      return <SessionPicker />
  }
}

function DefaultSession() {
  const [failure, setFailure] = useState("")

  useEffect(() => {
    parrotWeb.openDefaultSession({}).then(
      (session) => { navigateToSession(session.id, true) },
      (error: unknown) => { setFailure(ConnectError.from(error).message) },
    )
  }, [])

  return (
    <div className="flex h-dvh flex-col items-center justify-center gap-2 text-sm text-muted-foreground">
      {failure ? <span className="text-destructive">{failure}</span> : "Opening the latest session…"}
      {failure && <a href="#/" className="underline">Choose a session</a>}
    </div>
  )
}
