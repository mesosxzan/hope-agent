export type BackgroundJobsAutoOpenAction = "none" | "activate" | "open-in-background"

export interface BackgroundJobsAutoOpenInput {
  runningCount: number
  previousRunningCount: number
  dismissed: boolean
  activePanel: string | null
}

export function decideBackgroundJobsAutoOpen({
  runningCount,
  previousRunningCount,
  dismissed,
  activePanel,
}: BackgroundJobsAutoOpenInput): BackgroundJobsAutoOpenAction {
  if (dismissed || runningCount <= 0 || previousRunningCount > 0) return "none"
  if (activePanel === "background-jobs") return "none"
  if (!activePanel) return "activate"
  return "open-in-background"
}
