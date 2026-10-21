// The `$name` token under the caret, with the terminal CLI's rules (SkillCompletion.cs): the `$` must not follow a
// name character, and common environment variables are not skills.
const environmentNames = new Set(["HOME", "PATH", "USER", "SHELL", "PWD", "TMP", "TMPDIR", "TEMP", "LANG", "TERM", "XDG_CONFIG_HOME"])
const isName = (character: string | undefined) => character !== undefined && /[A-Za-z0-9_-]/.test(character)
const isBoundaryName = (character: string | undefined) => character !== undefined && /[A-Za-z0-9_]/.test(character)

export interface SkillToken {
  start: number
  end: number
  prefix: string
}

export function findSkillToken(text: string, caret: number): SkillToken | undefined {
  let start = caret
  while (start > 0 && isName(text[start - 1])) start--
  const dollar = start - 1
  if (text[dollar] !== "$" || isBoundaryName(text[dollar - 1])) return undefined
  let end = caret
  while (end < text.length && isName(text[end])) end++
  if (environmentNames.has(text.slice(start, end))) return undefined
  return { start: dollar, end, prefix: text.slice(start, caret) }
}
