import { Code, ConnectError } from "@connectrpc/connect"
import { useCallback, useEffect, useState } from "react"

import type { PendingQuestion } from "@/gen/parrot_pb"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { usePoll } from "@/lib/usePoll"
import { parrot } from "@/rpc/client"

interface QuestionPanelProps {
  userSessionId: string
  onFailure: (message: string) => void
}

export function QuestionPanel({ userSessionId, onFailure }: QuestionPanelProps) {
  const [pendingQuestions, setPendingQuestions] = useState<PendingQuestion[]>([])
  // The countdown runs down locally between polls, from the time the last poll answered.
  const [polledAt, setPolledAt] = useState(0)
  const [now, setNow] = useState(0)

  useEffect(() => {
    const timer = setInterval(() => { setNow(Date.now()) }, 1000)
    return () => { clearInterval(timer) }
  }, [])

  const poll = useCallback(
    () =>
      parrot.listPendingQuestions({ userSessionId }).then((response) => {
        setPendingQuestions(response.questions)
        setPolledAt(Date.now())
      }),
    [userSessionId],
  )
  usePoll(poll, false)

  return pendingQuestions.map((pending) => (
    <PendingQuestionCard
      key={pending.id}
      userSessionId={userSessionId}
      pending={pending}
      elapsedMilliseconds={Math.max(0, now - polledAt)}
      onFailure={onFailure}
    />
  ))
}

interface Draft {
  selected: string[]
  custom: string
}

function formatCountdown(milliseconds: number) {
  const seconds = Math.max(0, Math.ceil(milliseconds / 1000))
  return `Auto-return in ${String(Math.floor(seconds / 60))}:${String(seconds % 60).padStart(2, "0")}`
}

function PendingQuestionCard({
  userSessionId,
  pending,
  elapsedMilliseconds,
  onFailure,
}: QuestionPanelProps & { pending: PendingQuestion; elapsedMilliseconds: number }) {
  const [drafts, setDrafts] = useState<Draft[]>(() => pending.questions.map(() => ({ selected: [], custom: "" })))
  const [submitting, setSubmitting] = useState(false)

  function updateDraft(questionIndex: number, update: (draft: Draft) => Draft) {
    setDrafts((current) => current.map((draft, index) => (index === questionIndex ? update(draft) : draft)))
  }

  async function settle(send: () => Promise<unknown>) {
    setSubmitting(true)
    try {
      await send()
    } catch (error) {
      // Not found: already answered elsewhere or timed out, and the next poll removes it.
      const failure = ConnectError.from(error)
      if (failure.code !== Code.NotFound) onFailure(failure.message)
      setSubmitting(false)
    }
  }

  const answers = drafts.map((draft) => [...draft.selected, draft.custom.trim()].filter(Boolean).join(", "))

  return (
    <section className="mx-auto flex w-full max-w-4xl flex-col gap-3 px-4 pb-3">
      <div className="flex flex-col gap-3 rounded-lg border bg-card p-4 text-sm">
        {pending.questions.map((question, questionIndex) => (
          <div key={questionIndex} className="flex flex-col gap-2">
            {question.header && <div className="text-xs font-medium text-muted-foreground uppercase">{question.header}</div>}
            <div className="whitespace-pre-wrap">{question.prompt}</div>
            <div className="flex flex-wrap gap-2">
              {question.options.map((option) => {
                const chosen = drafts[questionIndex]?.selected.includes(option.label) ?? false
                return (
                  <Button
                    key={option.label}
                    size="sm"
                    className="h-auto flex-col items-start"
                    variant={chosen ? "default" : "outline"}
                    onClick={() => {
                      updateDraft(questionIndex, (draft) => ({
                        ...draft,
                        selected: chosen
                          ? draft.selected.filter((label) => label !== option.label)
                          : question.multiple ? [...draft.selected, option.label] : [option.label],
                      }))
                    }}
                  >
                    <span>{option.label}</span>
                    {option.description && <span className="text-xs font-normal whitespace-normal opacity-70">{option.description}</span>}
                  </Button>
                )
              })}
            </div>
            {question.custom && (
              <Input
                placeholder="Custom answer"
                value={drafts[questionIndex]?.custom ?? ""}
                onChange={(event) => { updateDraft(questionIndex, (draft) => ({ ...draft, custom: event.target.value })) }}
              />
            )}
          </div>
        ))}
        <div className="flex items-center gap-2">
          {pending.remainingTimeoutMs !== undefined && (
            <span className="text-xs text-muted-foreground">
              {formatCountdown(Number(pending.remainingTimeoutMs) - elapsedMilliseconds)}
            </span>
          )}
          <Button
            className="ml-auto"
            variant="outline"
            size="sm"
            disabled={submitting}
            onClick={() => void settle(() => parrot.rejectQuestion({ userSessionId, questionRequestId: pending.id }))}
          >
            Reject
          </Button>
          <Button
            size="sm"
            disabled={submitting || answers.some((answer) => !answer)}
            onClick={() =>
              void settle(() =>
                parrot.replyQuestion({ userSessionId, questionRequestId: pending.id, answers: answers.map((text) => ({ text })) }),
              )
            }
          >
            Answer
          </Button>
        </div>
      </div>
    </section>
  )
}
