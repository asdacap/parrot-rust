import { Loader2Icon } from "lucide-react"
import { useState } from "react"

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import { Command, CommandEmpty, CommandInput, CommandItem, CommandList } from "@/components/ui/command"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import type { SlashAnswer, SlashPromptFrame } from "@/slash/useSlashRunner"

interface SlashDialogHostProps {
  frame: SlashPromptFrame | undefined
  loadingActivity: string | null
  onAnswer: (answer: SlashAnswer) => void
}

const dismissed: SlashAnswer = { case: "dismissed", value: true }

export function SlashDialogHost({ frame, loadingActivity, onAnswer }: SlashDialogHostProps) {
  return (
    <>
      {loadingActivity !== null && (
        <div className="flex items-center gap-2 px-4 pb-2 text-xs text-muted-foreground">
          <Loader2Icon className="size-3 animate-spin" />
          {loadingActivity}
        </div>
      )}
      {frame && <SlashPromptDialog key={frame.value.promptId} frame={frame} onAnswer={onAnswer} />}
    </>
  )
}

function SlashPromptDialog({ frame, onAnswer }: { frame: SlashPromptFrame; onAnswer: (answer: SlashAnswer) => void }) {
  const [text, setText] = useState("")
  const dismissOnClose = (open: boolean) => { if (!open) onAnswer(dismissed) }

  switch (frame.case) {
    case "select":
      return (
        <Dialog open onOpenChange={dismissOnClose}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>{frame.value.title}</DialogTitle>
              <DialogDescription>Choose an option.</DialogDescription>
            </DialogHeader>
            <Command>
              <CommandInput placeholder="Filter…" />
              <CommandList>
                <CommandEmpty>No match.</CommandEmpty>
                {frame.value.options.map((option) => (
                  <CommandItem
                    key={option.id}
                    value={`${option.label} ${option.id}`}
                    onSelect={() => { onAnswer({ case: "optionId", value: option.id }) }}
                  >
                    <span>{option.label}</span>
                    <span className="truncate text-xs text-muted-foreground">{option.description}</span>
                  </CommandItem>
                ))}
              </CommandList>
            </Command>
          </DialogContent>
        </Dialog>
      )
    case "readText":
      return (
        <Dialog open onOpenChange={dismissOnClose}>
          <DialogContent>
            <form
              className="flex flex-col gap-4"
              onSubmit={(event) => {
                event.preventDefault()
                onAnswer({ case: "text", value: text })
              }}
            >
              <DialogHeader>
                <DialogTitle>{frame.value.prompt}</DialogTitle>
                <DialogDescription>Enter a value.</DialogDescription>
              </DialogHeader>
              <Input autoFocus type={frame.value.secret ? "password" : "text"} value={text} onChange={(event) => { setText(event.target.value) }} />
              <DialogFooter>
                <Button type="submit">OK</Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      )
    case "confirm":
      return (
        <AlertDialog open>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Confirm</AlertDialogTitle>
              <AlertDialogDescription asChild>
                <pre className="font-sans whitespace-pre-wrap">{frame.value.lines.join("\n")}</pre>
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel onClick={() => { onAnswer({ case: "confirmed", value: false }) }}>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={() => { onAnswer({ case: "confirmed", value: true }) }}>Continue</AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      )
    case "show":
      return (
        <Dialog open onOpenChange={dismissOnClose}>
          <DialogContent className="max-w-3xl">
            <DialogHeader>
              <DialogTitle>Output</DialogTitle>
            </DialogHeader>
            <pre className="max-h-96 overflow-auto font-mono text-xs whitespace-pre-wrap">{frame.value.lines.join("\n")}</pre>
            <DialogFooter>
              <Button onClick={() => { onAnswer(dismissed) }}>OK</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )
  }
}
