import { useCallback, useState } from "react"
import { useTranslation } from "react-i18next"

import { getTransport } from "@/lib/transport-provider"
import { useAudioRecorder } from "@/hooks/useAudioRecorder"
import type { RecorderState } from "@/hooks/useAudioRecorder"

interface Transcript {
  text: string
  language?: string | null
  durationMs?: number | null
}

export type VoiceInputState =
  | RecorderState
  | "transcribing"
  | "ready"

export interface UseVoiceInputResult {
  state: VoiceInputState
  durationMs: number
  audioLevel: number
  /** Human-readable last error (already localized). `null` when idle / OK. */
  errorMessage: string | null
  /** Begin recording. */
  start: () => Promise<void>
  /** Stop recording and run STT. Resolves with the transcript text (or empty on error). */
  stopAndTranscribe: () => Promise<string>
  /** Discard current recording without running STT. */
  cancel: () => void
  /** Clear the error message (called after the caller surfaced it). */
  clearError: () => void
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(reader.error ?? new Error("FileReader failed"))
    reader.onload = () => {
      const r = reader.result
      if (typeof r !== "string") {
        reject(new Error("Unexpected FileReader result"))
        return
      }
      const idx = r.indexOf(",")
      resolve(idx >= 0 ? r.slice(idx + 1) : r)
    }
    reader.readAsDataURL(blob)
  })
}

function deriveFilename(mimeType: string): string {
  if (mimeType.includes("webm")) return "voice.webm"
  if (mimeType.includes("ogg")) return "voice.ogg"
  if (mimeType.includes("mp4")) return "voice.m4a"
  if (mimeType.includes("mpeg")) return "voice.mp3"
  return "voice.bin"
}

/**
 * Composite hook: drives MediaRecorder + posts the final blob to
 * `stt_transcribe_blob`. Phase 4 ships the batch path (record-then-
 * transcribe); the streaming session API from Phase 2 is wired into
 * the hook in a later iteration.
 */
export function useVoiceInput(): UseVoiceInputResult {
  const { t } = useTranslation()
  const recorder = useAudioRecorder()
  const [transcribing, setTranscribing] = useState(false)
  const [errorMessage, setErrorMessage] = useState<string | null>(null)

  const start = useCallback(async () => {
    setErrorMessage(null)
    try {
      await recorder.start()
    } catch {
      // recorder.error fills in
    }
  }, [recorder])

  const stopAndTranscribe = useCallback(async (): Promise<string> => {
    try {
      const { blob, mimeType } = await recorder.stop()
      if (!blob.size) {
        setErrorMessage(t("voice.failed"))
        return ""
      }
      setTranscribing(true)
      const base64 = await blobToBase64(blob)
      const transcript = await getTransport().call<Transcript>("stt_transcribe_blob", {
        mimeType,
        filename: deriveFilename(mimeType),
        base64,
        options: {},
      })
      setTranscribing(false)
      if (!transcript || !transcript.text || !transcript.text.trim()) {
        setErrorMessage(t("voice.empty"))
        return ""
      }
      return transcript.text.trim()
    } catch (e) {
      setTranscribing(false)
      const msg = e instanceof Error ? e.message : String(e)
      // Backend `SttError::Display` always emits `stt:<code>: <body>`.
      // HTTP transport may wrap that inside `[HttpTransport] POST ... 400:
      // {"error":"stt:..."}` so scan anywhere in the message, not anchored.
      const code = msg.match(/stt:([a-z_]+):/)?.[1]
      if (code === "no_active_model") {
        setErrorMessage(t("voice.noProvider"))
      } else if (msg.toLowerCase().includes("permission")) {
        setErrorMessage(t("voice.permissionDenied"))
      } else {
        setErrorMessage(t("voice.failed"))
      }
      return ""
    }
  }, [recorder, t])

  const cancel = useCallback(() => {
    setErrorMessage(null)
    recorder.cancel()
  }, [recorder])

  const clearError = useCallback(() => setErrorMessage(null), [])

  const state: VoiceInputState = transcribing
    ? "transcribing"
    : recorder.state === "stopped"
      ? "ready"
      : recorder.state

  // Surface getUserMedia denial via i18n.
  const surfacedRecorderError =
    recorder.error && !errorMessage
      ? (() => {
          const name = (recorder.error as DOMException | Error).name ?? ""
          if (name === "NotAllowedError" || name === "SecurityError") {
            return t("voice.permissionDenied")
          }
          return t("voice.failed")
        })()
      : null

  return {
    state,
    durationMs: recorder.durationMs,
    audioLevel: recorder.audioLevel,
    errorMessage: errorMessage ?? surfacedRecorderError,
    start,
    stopAndTranscribe,
    cancel,
    clearError,
  }
}
