// How a tool call reads in the transcript, after the terminal CLI's tool presenters (Enhanced/Tools).
type Arguments = Record<string, unknown>

export function parseToolArguments(json: string): Arguments {
  try {
    const parsed: unknown = JSON.parse(json)
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed) ? (parsed as Arguments) : {}
  } catch {
    // Still streaming, or not JSON at all.
    return {}
  }
}

const text = (args: Arguments, key: string) => (typeof args[key] === "string" ? args[key] : "")

export interface QuestionArgument {
  prompt: string
  options: string[]
}

export function questionArguments(args: Arguments): QuestionArgument[] {
  const questions = Array.isArray(args.questions) ? (args.questions as unknown[]) : []
  return questions.map((question) => {
    const entry = typeof question === "object" && question !== null ? (question as Arguments) : {}
    const options = Array.isArray(entry.options) ? (entry.options as unknown[]) : []
    return {
      prompt: text(entry, "prompt") || "Question",
      options: options.map((option) => (typeof option === "object" && option !== null ? text(option as Arguments, "label") : "")),
    }
  })
}

export function toolLabel(name: string, args: Arguments): string {
  switch (name) {
    case "edit":
    case "write":
    case "read":
      return `${name} ${text(args, "path")}`
    case "glob": {
      const path = text(args, "path")
      return `glob "${text(args, "pattern")}"${path ? ` in ${path}` : ""}`
    }
    case "exec_command":
      return `$ ${text(args, "command") || text(args, "cmd")}`
    case "web_fetch":
      return `${text(args, "method") || "GET"} ${text(args, "url")}`
    case "agent_spawn":
      return `agent_spawn ${text(args, "name")}`
    case "wait":
      return "Wait for incoming activity"
    case "question": {
      const count = questionArguments(args).length
      return `Question · ${String(count)} ${count === 1 ? "item" : "items"}`
    }
    default:
      return name
  }
}

export function waitDurationMilliseconds(args: Arguments): number | undefined {
  return typeof args.duration_ms === "number" ? args.duration_ms : undefined
}

export function isUnifiedDiff(output: string): boolean {
  const [first, second] = output.split("\n", 2)
  return (first?.startsWith("--- ") ?? false) && (second?.startsWith("+++ ") ?? false)
}

export function tailLines(output: string, count: number): string {
  return output.trimEnd().split("\n").slice(-count).join("\n")
}
