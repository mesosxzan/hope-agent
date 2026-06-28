import { describe, expect, test } from "vitest"

import { isScrolledNearBottom, normalizeTerminalText, parseAnsiSegments } from "./terminalOutput"

describe("terminalOutput", () => {
  test("collapses carriage-return progress lines to the visible terminal line", () => {
    expect(normalizeTerminalText("Building [=>       ] 1/3\rBuilding [====>    ] 2/3\nDone")).toBe(
      "Building [====>    ] 2/3\nDone",
    )
  })

  test("preserves regular CRLF line endings", () => {
    expect(normalizeTerminalText("hello\r\nworld\r\n")).toBe("hello\nworld\n")
  })

  test("keeps SGR color spans and drops unsupported control sequences", () => {
    const segments = parseAnsiSegments("ok \x1b[31;1mfail\x1b[0m \x1b[?25lhidden")

    expect(segments).toEqual([
      { text: "ok ", className: undefined },
      { text: "fail", className: "font-semibold text-red-600 dark:text-red-400" },
      { text: " ", className: undefined },
      { text: "hidden", className: undefined },
    ])
  })

  test("detects whether the user is near the bottom of output", () => {
    expect(isScrolledNearBottom({ scrollHeight: 200, clientHeight: 100, scrollTop: 92 })).toBe(true)
    expect(isScrolledNearBottom({ scrollHeight: 200, clientHeight: 100, scrollTop: 40 })).toBe(
      false,
    )
  })
})
