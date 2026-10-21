import { ConnectError } from "@connectrpc/connect"
import { PlusIcon } from "lucide-react"
import { useEffect, useState } from "react"

import { SessionState, type SessionSummary } from "@/gen/parrot_pb"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { navigateToSession, sessionHash } from "@/lib/router"
import { describeHost, parrot } from "@/rpc/client"

export function SessionPicker() {
  const [workingDirectory, setWorkingDirectory] = useState("")
  const [sessions, setSessions] = useState<SessionSummary[]>([])
  const [failure, setFailure] = useState("")
  const [creating, setCreating] = useState(false)

  useEffect(() => {
    describeHost()
      .then(async (host) => {
        setWorkingDirectory(host.workingDirectory)
        const response = await parrot.listSessions({ workingDirectory: host.workingDirectory })
        setSessions(response.sessions.toSorted((left, right) => right.createdAt.localeCompare(left.createdAt)))
      })
      .catch((error: unknown) => { setFailure(ConnectError.from(error).message) })
  }, [])

  async function createSession() {
    setCreating(true)
    try {
      const created = await parrot.createSession({ interactivePermissions: true })
      navigateToSession(created.id)
    } catch (error) {
      setFailure(ConnectError.from(error).message)
      setCreating(false)
    }
  }

  return (
    <main className="mx-auto flex max-w-3xl flex-col gap-4 p-6">
      <header className="flex items-center gap-4">
        <div className="flex min-w-0 flex-col">
          <h1 className="text-lg font-semibold">Parrot sessions</h1>
          <p className="truncate font-mono text-xs text-muted-foreground">{workingDirectory}</p>
        </div>
        <Button className="ml-auto" disabled={creating} onClick={() => void createSession()}>
          <PlusIcon />
          New session
        </Button>
      </header>
      {failure && <p className="text-sm text-destructive">{failure}</p>}
      <ul className="flex flex-col divide-y rounded-md border">
        {sessions.map((session) => (
          <li key={session.userSessionId}>
            <a
              href={sessionHash(session.userSessionId)}
              className="flex items-center gap-3 px-3 py-2 text-sm hover:bg-accent"
            >
              <span className="truncate font-mono">{session.userSessionId}</span>
              <span className="truncate text-muted-foreground">{session.rootAgentName}</span>
              <span className="ml-auto truncate text-xs text-muted-foreground">{session.model}</span>
              <Badge variant={session.state === SessionState.ACTIVE ? "default" : "outline"}>
                {SessionState[session.state].toLowerCase()}
              </Badge>
              <span className="text-xs text-muted-foreground">{session.createdAt}</span>
            </a>
          </li>
        ))}
        {sessions.length === 0 && <li className="px-3 py-6 text-center text-sm text-muted-foreground">No sessions yet.</li>}
      </ul>
    </main>
  )
}
