import { Code, ConnectError } from "@connectrpc/connect"
import { ArrowLeftIcon } from "lucide-react"
import { useCallback, useEffect, useState } from "react"

import { Delivery, type UserSession } from "@/gen/parrot_pb"
import { Composer } from "@/components/Composer"
import { LiveActivity } from "@/components/LiveActivity"
import { PermissionDialog } from "@/components/PermissionDialog"
import { PlanCompletionDialog } from "@/components/PlanCompletionDialog"
import { QuestionPanel } from "@/components/QuestionPanel"
import { SlashDialogHost } from "@/components/SlashDialogHost"
import { StatusBar } from "@/components/StatusBar"
import { Timeline } from "@/components/Timeline"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { describeHost, parrot, parrotWeb } from "@/rpc/client"
import { useSessionEvents } from "@/session/useSessionEvents"
import { useSlashRunner } from "@/slash/useSlashRunner"

async function openSession(userSessionId: string): Promise<UserSession> {
  const { workingDirectory } = await describeHost()
  try {
    return await parrot.resumeSession({ userSessionId, workingDirectory, interactivePermissions: true })
  } catch (error) {
    if (ConnectError.from(error).code !== Code.AlreadyExists) throw error
    return await parrot.attachSession({ userSessionId, workingDirectory })
  }
}

export function SessionView({ userSessionId }: { userSessionId: string }) {
  const [session, setSession] = useState<UserSession | null>(null)
  const [failure, setFailure] = useState("")

  useEffect(() => {
    openSession(userSessionId).then(setSession, (error: unknown) => { setFailure(ConnectError.from(error).message) })
  }, [userSessionId])

  // A slash command reports only the model and mode it changed.
  const updateSession = useCallback((changed: UserSession) => {
    setSession((current) => (current?.id === changed.id ? { ...current, model: changed.model, mode: changed.mode } : current))
  }, [])

  async function cycleMode() {
    if (!session) return
    try {
      const { modes } = await parrot.listModes({})
      const next = modes[(modes.findIndex((mode) => mode.id === session.mode) + 1) % modes.length]
      if (next) updateSession(await parrot.updateSession({ userSessionId: session.id, mode: next.id }))
    } catch (error) {
      setFailure(ConnectError.from(error).message)
    }
  }

  return (
    <div className="flex h-dvh flex-col">
      <header className="flex items-center gap-3 border-b px-4 py-2 text-sm">
        <Button asChild variant="ghost" size="icon">
          <a href="#/" title="Sessions">
            <ArrowLeftIcon />
          </a>
        </Button>
        <span className="truncate font-mono">{userSessionId}</span>
      </header>
      {session ? (
        <SessionChat session={session} onCycleMode={() => void cycleMode()} onSessionChanged={updateSession} />
      ) : (
        <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
          {failure ? <span className="text-destructive">{failure}</span> : "Opening session…"}
        </div>
      )}
    </div>
  )
}

interface SessionChatProps {
  session: UserSession
  onCycleMode: () => void
  onSessionChanged: (session: UserSession) => void
}

function SessionChat({ session, onCycleMode, onSessionChanged }: SessionChatProps) {
  const userSessionId = session.id
  const { timeline, dispatch, connected, lost, tokenRate } = useSessionEvents(userSessionId)
  const slash = useSlashRunner(userSessionId, timeline.busy, dispatch, onSessionChanged)

  const reportFailure = (message: string) => { dispatch({ type: "error", message }) }

  function send(text: string, images: File[]) {
    if (images.length === 0 && text.startsWith("/")) {
      slash.submit(text)
      return
    }
    void (async () => {
      try {
        const uploaded = await Promise.all(
          images.map(async (image) => {
            const content = new Uint8Array(await image.arrayBuffer())
            const response = await parrotWeb.uploadAttachment({ userSessionId, displayName: image.name, mediaType: image.type, content })
            return response.artifact?.artifactId ?? ""
          }),
        )
        const parts = [
          ...(text ? [{ content: { case: "text" as const, value: text } }] : []),
          ...uploaded.map((artifactId) => ({ content: { case: "artifactId" as const, value: artifactId } })),
        ]
        await parrot.sendMessage({ userSessionId, parts, messageId: crypto.randomUUID(), delivery: timeline.busy ? Delivery.STEER : Delivery.QUEUE })
      } catch (error) {
        reportFailure(ConnectError.from(error).message)
      }
    })()
  }

  function interrupt() {
    parrot.interrupt({ userSessionId }).catch((error: unknown) => { reportFailure(ConnectError.from(error).message) })
  }

  if (lost) {
    return <div className="flex flex-1 items-center justify-center text-sm text-destructive">{lost}</div>
  }

  return (
    <>
      <Timeline items={timeline.items} agents={timeline.agents} />
      <LiveActivity queues={timeline.queues} processes={timeline.processes} taskProgress={timeline.taskProgress} />
      {slash.queued.length > 0 && (
        <div className="mx-auto flex w-full max-w-4xl flex-wrap items-center gap-2 px-4 pb-2 text-xs text-muted-foreground">
          waiting for the turn to finish:
          {slash.queued.map((text, index) => (
            <Badge key={index} variant="outline">{text}</Badge>
          ))}
        </div>
      )}
      <QuestionPanel userSessionId={userSessionId} onFailure={reportFailure} />
      <SlashDialogHost frame={slash.prompt?.frame} loadingActivity={slash.loadingActivity} onAnswer={(answer) => void slash.answer(answer)} />
      <PermissionDialog userSessionId={userSessionId} agents={timeline.agents} onFailure={reportFailure} />
      {!timeline.busy && timeline.pendingPlan?.dialog && (
        <PlanCompletionDialog
          userSessionId={userSessionId}
          dialog={timeline.pendingPlan.dialog}
          onSettled={() => { dispatch({ type: "planSettled" }) }}
          onSessionChanged={onSessionChanged}
          onFailure={reportFailure}
        />
      )}
      <Composer userSessionId={userSessionId} busy={timeline.busy} onSend={send} onInterrupt={interrupt} onCycleMode={onCycleMode} />
      <StatusBar
        session={session}
        modelIcon={timeline.modelIcon}
        activity={timeline.activity}
        busy={timeline.busy}
        usage={timeline.usage}
        tokenRate={tokenRate}
        connected={connected}
        onCycleMode={onCycleMode}
      />
    </>
  )
}
