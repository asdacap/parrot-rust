import { ConnectError } from "@connectrpc/connect"
import { useState } from "react"

import { Delivery, type DialogChoice, type TurnCompleteDialog, type UserSession } from "@/gen/parrot_pb"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Textarea } from "@/components/ui/textarea"
import { parrot } from "@/rpc/client"

interface PlanCompletionDialogProps {
  userSessionId: string
  dialog: TurnCompleteDialog
  onSettled: () => void
  onSessionChanged: (session: UserSession) => void
  onFailure: (message: string) => void
}

// The terminal CLI's plan dialog: a choice switches mode and sends its prompt; the custom choice sends feedback instead.
export function PlanCompletionDialog({ userSessionId, dialog, onSettled, onSessionChanged, onFailure }: PlanCompletionDialogProps) {
  const [writingFeedback, setWritingFeedback] = useState(false)
  const [feedback, setFeedback] = useState("")

  async function settle(act: () => Promise<void>) {
    onSettled()
    try {
      await act()
    } catch (error) {
      onFailure(ConnectError.from(error).message)
    }
  }

  const send = async (text: string) => {
    await parrot.sendMessage({ userSessionId, text, messageId: crypto.randomUUID(), delivery: Delivery.QUEUE })
  }

  const choose = (choice: DialogChoice) =>
    settle(async () => {
      if (choice.action?.mode) onSessionChanged(await parrot.updateSession({ userSessionId, mode: choice.action.mode }))
      if (choice.action?.prompt) await send(choice.action.prompt)
    })

  return (
    <Dialog open onOpenChange={(open) => { if (!open) onSettled() }}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{dialog.prompt}</DialogTitle>
          <DialogDescription className="whitespace-pre-wrap">{dialog.context.join("\n")}</DialogDescription>
        </DialogHeader>
        {writingFeedback ? (
          <form
            className="flex flex-col gap-2"
            onSubmit={(event) => {
              event.preventDefault()
              if (feedback.trim()) void settle(() => send(feedback.trim()))
            }}
          >
            <Textarea autoFocus placeholder={dialog.customPrompt} value={feedback} onChange={(event) => { setFeedback(event.target.value) }} />
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => { setWritingFeedback(false) }}>
                Back
              </Button>
              <Button type="submit" disabled={!feedback.trim()}>
                Send
              </Button>
            </DialogFooter>
          </form>
        ) : (
          <div className="flex flex-col gap-2">
            {dialog.choices.map((choice) => (
              <Button key={choice.value} variant="outline" className="h-auto flex-col items-start" onClick={() => void choose(choice)}>
                <span>{choice.value}</span>
                {choice.description && <span className="text-xs font-normal whitespace-normal text-muted-foreground">{choice.description}</span>}
              </Button>
            ))}
            {dialog.customChoice && (
              <Button variant="outline" className="h-auto flex-col items-start" onClick={() => { setWritingFeedback(true) }}>
                <span>{dialog.customChoice}</span>
                <span className="text-xs font-normal whitespace-normal text-muted-foreground">
                  {dialog.customDescription || "Provide feedback and revise"}
                </span>
              </Button>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}
