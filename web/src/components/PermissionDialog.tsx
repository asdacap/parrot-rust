import { Code, ConnectError } from "@connectrpc/connect"
import { useCallback, useState } from "react"

import {
  PermissionAction,
  PermissionTargetKind,
  PermissionTargetScope,
  type PendingPermission,
  type PermissionChoice,
} from "@/gen/parrot_pb"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { usePoll } from "@/lib/usePoll"
import { parrot } from "@/rpc/client"
import type { AgentInfo } from "@/session/timeline"

interface PermissionDialogProps {
  userSessionId: string
  agents: ReadonlyMap<string, AgentInfo>
  onFailure: (message: string) => void
}

export function PermissionDialog({ userSessionId, agents, onFailure }: PermissionDialogProps) {
  const [pending, setPending] = useState<PendingPermission[]>([])
  const [reasonChoice, setReasonChoice] = useState<PermissionChoice | null>(null)
  const [reason, setReason] = useState("")

  // Polled, as the terminal CLI does, so a permission answered elsewhere or timed out closes here too.
  const poll = useCallback(
    () => parrot.listPendingPermissions({ userSessionId }).then((response) => { setPending(response.permissions) }),
    [userSessionId],
  )
  usePoll(poll, true)

  const permission = pending[0]
  if (!permission) return null
  const agent = agents.get(permission.agentSessionId)
  const plainReject = permission.choices.find((choice) => choice.action === PermissionAction.DENY && !choice.requiresReason)

  async function reply(choice: PermissionChoice | undefined, withReason = "") {
    setReasonChoice(null)
    setReason("")
    if (!permission || !choice) return
    setPending((current) => current.filter((item) => item.id !== permission.id))
    try {
      await parrot.replyPermission({ userSessionId, permissionRequestId: permission.id, choiceValue: choice.value, reason: withReason })
    } catch (error) {
      const failure = ConnectError.from(error)
      if (failure.code !== Code.NotFound) onFailure(failure.message)
    }
  }

  return (
    <Dialog open>
      <DialogContent
        onEscapeKeyDown={(event) => {
          event.preventDefault()
          void reply(plainReject)
        }}
        onInteractOutside={(event) => { event.preventDefault() }}
      >
        <DialogHeader>
          <DialogTitle>Permission requested</DialogTitle>
          <DialogDescription>
            {permission.reason}
            {agent && ` (from subagent ${agent.name})`}
          </DialogDescription>
        </DialogHeader>
        <ul className="flex flex-col gap-1 font-mono text-xs">
          {permission.targets.map((target) => (
            <li key={target.path} className="truncate">
              {target.scope === PermissionTargetScope.WRITE && "write "}
              {target.kind === PermissionTargetKind.DIRECTORY ? "dir " : "file "}
              {target.path}
            </li>
          ))}
        </ul>
        {reasonChoice ? (
          <form
            className="flex flex-col gap-2"
            onSubmit={(event) => {
              event.preventDefault()
              if (reason.trim()) void reply(reasonChoice, reason.trim())
            }}
          >
            <Input autoFocus placeholder="Reason" value={reason} onChange={(event) => { setReason(event.target.value) }} />
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => void reply(plainReject)}>
                Cancel
              </Button>
              <Button type="submit" disabled={!reason.trim()}>
                {reasonChoice.label}
              </Button>
            </DialogFooter>
          </form>
        ) : (
          <DialogFooter>
            {permission.choices.map((choice) => (
              <Button
                key={choice.value}
                variant={choice.action === PermissionAction.DENY ? "outline" : "default"}
                onClick={() => {
                  if (choice.requiresReason) setReasonChoice(choice)
                  else void reply(choice)
                }}
              >
                {choice.label}
              </Button>
            ))}
          </DialogFooter>
        )}
      </DialogContent>
    </Dialog>
  )
}
