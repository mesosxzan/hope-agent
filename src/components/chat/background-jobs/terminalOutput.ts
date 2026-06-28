export interface TerminalSegment {
  text: string
  className?: string
}

interface AnsiStyle {
  fg?: string
  bold: boolean
  dim: boolean
}

const ANSI_ESCAPE = "\x1b["

const ANSI_COLOR_CLASS: Record<string, string> = {
  "30": "text-neutral-700 dark:text-neutral-300",
  "31": "text-red-600 dark:text-red-400",
  "32": "text-emerald-600 dark:text-emerald-400",
  "33": "text-amber-600 dark:text-amber-300",
  "34": "text-blue-600 dark:text-blue-400",
  "35": "text-fuchsia-600 dark:text-fuchsia-400",
  "36": "text-cyan-600 dark:text-cyan-300",
  "37": "text-neutral-600 dark:text-neutral-200",
  "90": "text-neutral-500 dark:text-neutral-400",
  "91": "text-red-500 dark:text-red-300",
  "92": "text-emerald-500 dark:text-emerald-300",
  "93": "text-amber-500 dark:text-amber-200",
  "94": "text-blue-500 dark:text-blue-300",
  "95": "text-fuchsia-500 dark:text-fuchsia-300",
  "96": "text-cyan-500 dark:text-cyan-200",
  "97": "text-neutral-700 dark:text-white",
}

function readAnsiSequence(
  input: string,
  start: number,
): { sequence: string; final: string; end: number } | null {
  if (!input.startsWith(ANSI_ESCAPE, start)) return null
  let end = start + ANSI_ESCAPE.length
  while (end < input.length) {
    const code = input.charCodeAt(end)
    if (code >= 0x40 && code <= 0x7e) {
      return {
        sequence: input.slice(start, end + 1),
        final: input[end],
        end: end + 1,
      }
    }
    end += 1
  }
  return null
}

export function normalizeTerminalText(input: string): string {
  const lines: string[] = []
  let current = ""

  for (let i = 0; i < input.length; ) {
    const ansi = readAnsiSequence(input, i)
    if (ansi) {
      if (ansi.final === "m") current += ansi.sequence
      if (ansi.final === "K") current = ""
      i = ansi.end
      continue
    }

    const ch = input[i]
    if (ch === "\r") {
      if (input[i + 1] === "\n") {
        lines.push(current)
        current = ""
        i += 2
        continue
      }
      current = ""
      i += 1
      continue
    }
    if (ch === "\n") {
      lines.push(current)
      current = ""
      i += 1
      continue
    }

    current += ch
    i += 1
  }

  lines.push(current)
  return lines.join("\n")
}

function styleClassName(style: AnsiStyle): string | undefined {
  const classes = [
    style.bold ? "font-semibold" : null,
    style.dim ? "opacity-70" : null,
    style.fg ? ANSI_COLOR_CLASS[style.fg] : null,
  ].filter(Boolean)
  return classes.length > 0 ? classes.join(" ") : undefined
}

function applySgr(style: AnsiStyle, params: string): AnsiStyle {
  const codes = (params.trim() ? params.split(";") : ["0"]).map((raw) => raw || "0")
  let next = { ...style }

  for (const code of codes) {
    switch (code) {
      case "0":
        next = { bold: false, dim: false }
        break
      case "1":
        next.bold = true
        break
      case "2":
        next.dim = true
        break
      case "22":
        next.bold = false
        next.dim = false
        break
      case "39":
        next.fg = undefined
        break
      default:
        if (ANSI_COLOR_CLASS[code]) next.fg = code
        break
    }
  }

  return next
}

export function parseAnsiSegments(input: string): TerminalSegment[] {
  const segments: TerminalSegment[] = []
  let style: AnsiStyle = { bold: false, dim: false }
  let buffer = ""

  const flush = () => {
    if (!buffer) return
    segments.push({ text: buffer, className: styleClassName(style) })
    buffer = ""
  }

  for (let i = 0; i < input.length; ) {
    const ansi = readAnsiSequence(input, i)
    if (ansi) {
      flush()
      if (ansi.final === "m") {
        style = applySgr(style, ansi.sequence.slice(ANSI_ESCAPE.length, -1))
      }
      i = ansi.end
      continue
    }

    buffer += input[i]
    i += 1
  }

  flush()
  return segments
}

export function isScrolledNearBottom(
  node: Pick<HTMLElement, "scrollHeight" | "clientHeight" | "scrollTop">,
): boolean {
  return node.scrollHeight - node.clientHeight - node.scrollTop <= 8
}
