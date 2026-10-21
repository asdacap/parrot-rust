import { ImageIcon, SendIcon, SquareIcon, XIcon } from "lucide-react"
import { useEffect, useLayoutEffect, useRef, useState, type ClipboardEvent, type DragEvent, type KeyboardEvent } from "react"

import type { Skill } from "@/gen/parrot_pb"
import type { SlashCommand } from "@/gen/web_pb"
import { Button } from "@/components/ui/button"
import { Command, CommandItem, CommandList } from "@/components/ui/command"
import { Textarea } from "@/components/ui/textarea"
import { findSkillToken } from "@/lib/skillToken"
import { parrot, parrotWeb } from "@/rpc/client"

const historyLimit = 100
// What the server accepts as an image attachment.
const imageTypes = ["image/png", "image/jpeg", "image/gif", "image/webp"]

interface ComposerProps {
  userSessionId: string
  busy: boolean
  onSend: (text: string, images: File[]) => void
  onInterrupt: () => void
  onCycleMode: () => void
}

interface Completion {
  name: string
  summary: string
}

const historyKey = (userSessionId: string) => `parrot.history.${userSessionId}`

function loadHistory(userSessionId: string): string[] {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(historyKey(userSessionId)) ?? "[]")
    return Array.isArray(stored) ? stored.filter((entry): entry is string => typeof entry === "string") : []
  } catch {
    return []
  }
}

function saveHistory(userSessionId: string, history: string[]) {
  try {
    localStorage.setItem(historyKey(userSessionId), JSON.stringify(history))
  } catch {
    // Only a convenience; the next submit tries again.
  }
}

export function Composer({ userSessionId, busy, onSend, onInterrupt, onCycleMode }: ComposerProps) {
  const textarea = useRef<HTMLTextAreaElement>(null)
  const pendingCaret = useRef<number | null>(null)
  const [text, setText] = useState("")
  const [caret, setCaret] = useState(0)
  const [commands, setCommands] = useState<SlashCommand[]>([])
  const [skills, setSkills] = useState<Skill[]>([])
  const [highlighted, setHighlighted] = useState("")
  const [history, setHistory] = useState(() => loadHistory(userSessionId))
  // How far back Up has gone; 0 is the draft being written.
  const [recalled, setRecalled] = useState(0)
  const [draft, setDraft] = useState("")
  const [images, setImages] = useState<File[]>([])
  const picker = useRef<HTMLInputElement>(null)
  // One interrupt per turn, as in the terminal CLI.
  const [interruptSent, setInterruptSent] = useState(false)
  if (!busy && interruptSent) setInterruptSent(false)

  useEffect(() => {
    parrotWeb.listSlashCommands({}).then((response) => { setCommands(response.commands) }, () => undefined)
  }, [])

  const typingCommand = text.startsWith("/") && !/\s/.test(text)
  const skillToken = typingCommand ? undefined : findSkillToken(text, caret)
  const typingSkill = skillToken !== undefined

  // Refetched whenever a `$` token starts, so a skill enabled by /skills or /mode shows up.
  useEffect(() => {
    if (!typingSkill) return
    parrot.listSkills({ userSessionId }).then(
      (response) => {
        const enabled = response.skills.filter((skill) => skill.enabled).toSorted((left, right) => left.name.localeCompare(right.name))
        setSkills(enabled.filter((skill, index) => enabled.findIndex((other) => other.name === skill.name) === index))
      },
      () => { setSkills([]) },
    )
  }, [typingSkill, userSessionId])

  useLayoutEffect(() => {
    if (pendingCaret.current === null) return
    textarea.current?.setSelectionRange(pendingCaret.current, pendingCaret.current)
    setCaret(pendingCaret.current)
    pendingCaret.current = null
  }, [text])

  const matches: Completion[] = typingCommand
    ? commands.filter((command) => command.name.startsWith(text)).map(({ name, summary }) => ({ name, summary }))
    : skillToken
      ? skills
          .filter((skill) => skill.name.toLowerCase().startsWith(skillToken.prefix.toLowerCase()))
          .map((skill) => ({ name: `$${skill.name}`, summary: skill.shortDescription ?? skill.description }))
      : []
  const selectedName = matches.some((completion) => completion.name === highlighted) ? highlighted : matches[0]?.name

  function replaceText(next: string, nextCaret = next.length) {
    pendingCaret.current = nextCaret
    setText(next)
  }

  function accepted(name: string) {
    if (skillToken) return { text: text.slice(0, skillToken.start) + name + text.slice(skillToken.end), caret: skillToken.start + name.length }
    return { text: `${name} `, caret: name.length + 1 }
  }

  function attach(files: Iterable<File>) {
    const accepted = [...files].filter((file) => imageTypes.includes(file.type))
    if (accepted.length > 0) setImages((current) => [...current, ...accepted])
    return accepted.length > 0
  }

  function submit(submitted: string) {
    if (!submitted.trim() && images.length === 0) return
    onSend(submitted, images)
    setImages([])
    const nextHistory = [...history.filter((entry) => entry !== submitted), submitted].slice(-historyLimit)
    setHistory(nextHistory)
    saveHistory(userSessionId, nextHistory)
    setRecalled(0)
    replaceText("")
  }

  function interrupt() {
    if (interruptSent) return
    setInterruptSent(true)
    onInterrupt()
  }

  function moveHighlight(offset: number) {
    const index = matches.findIndex((completion) => completion.name === selectedName)
    const target = matches[(index + offset + matches.length) % matches.length]
    if (target) setHighlighted(target.name)
  }

  function recall(index: number) {
    if (recalled === 0) setDraft(text)
    setRecalled(index)
    replaceText(index === 0 ? draft : (history[history.length - index] ?? ""))
  }

  function choose(name: string) {
    if (typingCommand) submit(name)
    else submit(accepted(name).text)
  }

  function handleKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    const onFirstLine = !text.slice(0, caret).includes("\n")
    const onLastLine = !text.slice(caret).includes("\n")
    if (event.key === "Tab" && event.shiftKey) onCycleMode()
    else if (event.key === "Escape" && busy) interrupt()
    else if (selectedName !== undefined && event.key === "ArrowDown") moveHighlight(1)
    else if (selectedName !== undefined && event.key === "ArrowUp") moveHighlight(-1)
    else if (selectedName !== undefined && event.key === "Tab") {
      const next = accepted(selectedName)
      replaceText(next.text, next.caret)
    } else if (event.key === "ArrowUp" && onFirstLine && recalled < history.length) recall(recalled + 1)
    else if (event.key === "ArrowDown" && onLastLine && recalled > 0) recall(recalled - 1)
    else if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      if (selectedName === undefined) submit(text)
      else choose(selectedName)
    } else return
    event.preventDefault()
  }

  return (
    <div
      className="relative mx-auto flex w-full max-w-4xl flex-wrap items-end gap-2 px-4 pb-3"
      onDragOver={(event: DragEvent) => { event.preventDefault() }}
      onDrop={(event: DragEvent) => {
        event.preventDefault()
        attach(event.dataTransfer.files)
      }}
    >
      {images.length > 0 && (
        <div className="flex w-full flex-wrap gap-2">
          {images.map((image, index) => (
            <span key={index} className="flex items-center gap-1 rounded-md border px-2 py-0.5 text-xs">
              <ImageIcon className="size-3" />
              {image.name}
              <Button
                variant="ghost"
                size="icon"
                title="Remove"
                onClick={() => { setImages((current) => current.filter((_, other) => other !== index)) }}
              >
                <XIcon />
              </Button>
            </span>
          ))}
        </div>
      )}
      {selectedName !== undefined && (
        <div className="absolute bottom-full left-4 w-96 rounded-md border shadow-md">
          <Command value={selectedName} onValueChange={setHighlighted} shouldFilter={false}>
            <CommandList>
              {matches.map((completion) => (
                <CommandItem key={completion.name} value={completion.name} onSelect={choose}>
                  <span className="font-mono">{completion.name}</span>
                  <span className="truncate text-xs text-muted-foreground">{completion.summary}</span>
                </CommandItem>
              ))}
            </CommandList>
          </Command>
        </div>
      )}
      <Textarea
        ref={textarea}
        autoFocus
        className="max-h-60 flex-1"
        placeholder="Message, / for commands, $ for skills. Enter sends, Shift+Enter adds a line, Shift+Tab switches mode, Esc stops."
        value={text}
        onChange={(event) => {
          setText(event.target.value)
          setCaret(event.target.selectionStart)
        }}
        onSelect={(event) => { setCaret(event.currentTarget.selectionStart) }}
        onPaste={(event: ClipboardEvent) => { if (attach(event.clipboardData.files)) event.preventDefault() }}
        onKeyDown={handleKeyDown}
      />
      <input
        ref={picker}
        type="file"
        accept={imageTypes.join(",")}
        multiple
        hidden
        onChange={(event) => {
          attach(event.target.files ?? [])
          event.target.value = ""
        }}
      />
      <Button variant="outline" size="icon" title="Attach images" onClick={() => picker.current?.click()}>
        <ImageIcon />
      </Button>
      {busy ? (
        <Button variant="destructive" size="icon" title="Stop (Esc)" disabled={interruptSent} onClick={interrupt}>
          <SquareIcon />
        </Button>
      ) : (
        <Button size="icon" title="Send" onClick={() => { submit(text) }}>
          <SendIcon />
        </Button>
      )}
    </div>
  )
}
