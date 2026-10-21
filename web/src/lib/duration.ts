// The terminal CLI's AgentDurationFormatter: whole seconds, rounded, as "5s", "1m 05s" or "1h 02m 03s".
export function formatDuration(milliseconds: number): string {
  const totalSeconds = Math.floor(Math.max(0, milliseconds + 500) / 1000)
  const pad = (value: number) => String(value).padStart(2, "0")
  if (totalSeconds < 60) return `${String(totalSeconds)}s`
  if (totalSeconds < 3600) return `${String(Math.floor(totalSeconds / 60))}m ${pad(totalSeconds % 60)}s`
  return `${String(Math.floor(totalSeconds / 3600))}h ${pad(Math.floor(totalSeconds / 60) % 60)}m ${pad(totalSeconds % 60)}s`
}
