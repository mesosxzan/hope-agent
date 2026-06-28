import { describe, expect, test } from "vitest"

import { decideBackgroundJobsAutoOpen } from "./autoOpenPolicy"

describe("decideBackgroundJobsAutoOpen", () => {
  test("activates the panel when a new background job appears and no right panel is active", () => {
    expect(
      decideBackgroundJobsAutoOpen({
        runningCount: 1,
        previousRunningCount: 0,
        dismissed: false,
        activePanel: null,
      }),
    ).toBe("activate")
  })

  test("opens in the background when the user is already looking at another right panel", () => {
    expect(
      decideBackgroundJobsAutoOpen({
        runningCount: 1,
        previousRunningCount: 0,
        dismissed: false,
        activePanel: "preview",
      }),
    ).toBe("open-in-background")
  })

  test("does nothing after dismissal or when jobs were already running", () => {
    expect(
      decideBackgroundJobsAutoOpen({
        runningCount: 1,
        previousRunningCount: 0,
        dismissed: true,
        activePanel: null,
      }),
    ).toBe("none")
    expect(
      decideBackgroundJobsAutoOpen({
        runningCount: 2,
        previousRunningCount: 1,
        dismissed: false,
        activePanel: null,
      }),
    ).toBe("none")
  })
})
