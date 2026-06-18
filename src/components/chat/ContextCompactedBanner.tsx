import { useTranslation } from "react-i18next"
import { Archive, Loader2, AlertCircle } from "lucide-react"
import { cn } from "@/lib/utils"
import type { ContextCompactedEvent, ContextCompactionProgressEvent } from "@/types/chat"

/** Inline context-compaction banner — muted gray chip. Distinct from the
 *  amber failover family: compaction is a normal operational event, not a
 *  failure. Tier 0/1 micro-compactions are filtered at the backend persist
 *  layer (see chat_engine/persister.rs); this banner only sees Tier ≥ 2. */
export default function ContextCompactedBanner({
  event,
}: {
  event: ContextCompactedEvent & ContextCompactionProgressEvent
}) {
  const { t } = useTranslation()
  const phase = event.phase
  const isRunning =
    phase === "preparing" ||
    phase === "summarizing" ||
    phase === "restoring_files" ||
    phase === "preserving_runtime_state" ||
    phase === "finalizing" ||
    event.description === "summarizing" ||
    event.description === "emergency_compacting"
  const isFailed = phase === "failed"
  const isEmergency =
    event.kind === "emergency" ||
    event.description === "emergency_compacting" ||
    event.tier_applied === 4
  const summarizedMsgs =
    typeof event.messages_to_summarize === "number" ? event.messages_to_summarize : undefined
  const affectedMsgs =
    typeof event.messages_affected === "number" && event.messages_affected > 0
      ? event.messages_affected
      : undefined
  const msgs =
    isRunning || event.tier_applied === 3
      ? summarizedMsgs ?? affectedMsgs
      : affectedMsgs ?? summarizedMsgs
  const Icon = isFailed ? AlertCircle : isRunning ? Loader2 : Archive
  const title = (() => {
    if (isFailed) return t("chat.contextCompactionFailedTitle")
    if (isRunning) {
      if (isEmergency) return t("chat.contextCompaction.emergency")
      if (phase === "summarizing" || event.description === "summarizing") {
        return t("chat.contextCompaction.summarizing")
      }
      if (phase === "restoring_files") return t("chat.contextCompaction.restoringFiles")
      if (phase === "preserving_runtime_state") return t("chat.contextCompaction.preserveRuntime")
      if (phase === "finalizing") return t("chat.contextCompaction.finalizing")
      return t("chat.contextCompaction.preparing")
    }
    if (isEmergency) return t("chat.contextCompaction.emergencyDone")
    if (event.tier_applied === 3 || event.description === "summarization_needed") {
      return t("chat.contextCompaction.summaryDone")
    }
    return t("chat.contextCompactedTitle")
  })()

  const subtitle = (() => {
    const parts: string[] = []
    if (typeof msgs === "number") {
      parts.push(t("chat.contextCompaction.messages", { count: msgs }))
    }
    if (typeof event.files_recovered === "number" && event.files_recovered > 0) {
      parts.push(t("chat.contextCompaction.filesRecovered", { count: event.files_recovered }))
    }
    return parts.join(" · ")
  })()

  return (
    <div
      className={cn(
        "mb-1.5 inline-flex max-w-full items-center gap-1.5 rounded-full border px-2.5 py-1 text-[11px]",
        isFailed
          ? "border-amber-500/25 bg-amber-500/10 text-amber-700 dark:text-amber-300"
          : isRunning
            ? "border-blue-500/25 bg-blue-500/10 text-blue-700 dark:text-blue-300"
            : "border-border/60 bg-muted/40 text-muted-foreground",
      )}
    >
      <Icon className={cn("h-3 w-3 shrink-0 opacity-70", isRunning && "animate-spin")} />
      <span className="shrink-0 font-medium text-foreground/75">
        {title}
      </span>
      {subtitle && (
        <>
          <span className="shrink-0 opacity-30">·</span>
          <span className="truncate opacity-70">{subtitle}</span>
        </>
      )}
    </div>
  )
}
