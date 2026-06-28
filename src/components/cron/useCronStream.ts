import { useCallback, useEffect, useRef } from "react"
import { getTransport } from "@/lib/transport-provider"
import { reloadAndMergeSessionMessages } from "@/components/chat/chatUtils"
import {
  createStreamDeltaBuffers,
  discardAllPendingStreamDeltas,
  discardPendingStreamDeltas,
  handleStreamEvent,
  streamCursorKey,
} from "@/components/chat/hooks/useStreamEventHandler"
import type { Message } from "@/types/chat"

const PAGE_SIZE = 50

// Backend event names — see `crates/ha-core/src/chat_engine/stream_broadcast.rs`.
const EVENT_CHAT_STREAM_DELTA = "chat:stream_delta"
const EVENT_CHAT_STREAM_END = "chat:stream_end"
const EVENT_CHAT_TURN_STARTED = "chat:turn_started"

interface SessionStreamState {
  active: boolean
  lastSeq: number
  streamId?: string | null
  turnId?: string | null
}

interface UseCronStreamParams {
  sessionId: string
  messages: Message[]
  setMessages: React.Dispatch<React.SetStateAction<Message[]>>
}

/**
 * Enables real-time streaming output for a cron session viewer.
 *
 * Mirrors the streaming contract used by `ChatScreen` (`useChatStreamReattach`
 * + `useChannelStreaming`) but simplified for a read-only cron viewer:
 *  - No user input, no `startChat`, no `__pending__` session creation.
 *  - No sidebar / session-list refresh.
 *  - Single session bound to this component.
 *
 * On mount, checks `get_session_stream_state` to detect an already-running
 * stream and seeds the `lastSeq` cursor so we skip events already persisted
 * to the DB snapshot loaded by the initial fetch.
 *
 * Event flow:
 *  `chat:stream_delta`  → `handleStreamEvent` (incremental text/tool/usage)
 *  `chat:stream_end`    → DB reload (final reconciliation)
 *  `chat:turn_started`  → insert streaming placeholder, mark loading
 */
export function useCronStream({ sessionId, messages, setMessages }: UseCronStreamParams): void {
  const sessionCacheRef = useRef<Map<string, Message[]>>(new Map())
  const lastSeqRef = useRef<Map<string, number>>(new Map())
  const deltaBuffersRef = useRef(createStreamDeltaBuffers())
  const isStreamingRef = useRef(false)

  // Keep the cache in sync with the latest messages state.
  sessionCacheRef.current.set(sessionId, messages)

  const updateSessionMessages = useCallback(
    (sid: string, updater: (prev: Message[]) => Message[]) => {
      if (sid !== sessionId) return
      const prev = sessionCacheRef.current.get(sid) || []
      const next = updater(prev)
      sessionCacheRef.current.set(sid, next)
      setMessages(next)
    },
    [sessionId, setMessages],
  )

  const appendStreamingPlaceholder = useCallback((msgs: Message[]): Message[] => {
    const last = msgs[msgs.length - 1]
    if (last?.role === "assistant" && last.isStreaming) return msgs
    return [
      ...msgs,
      {
        role: "assistant" as const,
        content: "",
        isStreaming: true,
        timestamp: new Date().toISOString(),
      },
    ]
  }, [])

  // On mount or session switch, check if there's an active stream and seed the cursor.
  useEffect(() => {
    let cancelled = false

    getTransport()
      .call<SessionStreamState>("get_session_stream_state", { sessionId })
      .then((state) => {
        if (cancelled || !state) return

        if (state.active) {
          isStreamingRef.current = true
          const streamId = state.streamId || undefined
          const cursorKey = streamCursorKey(sessionId, streamId)
          if (!lastSeqRef.current.has(cursorKey)) {
            lastSeqRef.current.set(cursorKey, Number(state.lastSeq) || 0)
          }
          // Add a streaming placeholder to the current messages.
          setMessages((prev) => {
            const withPlaceholder = appendStreamingPlaceholder(prev)
            sessionCacheRef.current.set(sessionId, withPlaceholder)
            return withPlaceholder
          })
        }
      })
      .catch(() => {
        // Older backend without this command — gracefully degrade.
      })

    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId])

  // Listen for `chat:turn_started` — insert streaming placeholder.
  useEffect(() => {
    const unlisten = getTransport().listen(EVENT_CHAT_TURN_STARTED, (raw) => {
      const payload = raw as { sessionId?: string; turnId?: string } | null
      if (!payload?.sessionId || payload.sessionId !== sessionId) return

      isStreamingRef.current = true
      setMessages((prev) => {
        const withPlaceholder = appendStreamingPlaceholder(prev)
        sessionCacheRef.current.set(sessionId, withPlaceholder)
        return withPlaceholder
      })
    })
    return unlisten
  }, [sessionId, appendStreamingPlaceholder, setMessages])

  // Listen for `chat:stream_delta` — incremental updates via `handleStreamEvent`.
  useEffect(() => {
    const unlisten = getTransport().listen(EVENT_CHAT_STREAM_DELTA, (raw) => {
      const payload = raw as { sessionId?: string; seq?: number; streamId?: string; event?: string }
      if (!payload?.sessionId || payload.sessionId !== sessionId) return
      if (typeof payload.seq !== "number") return

      const seq = payload.seq
      const cursorKey = streamCursorKey(sessionId, payload.streamId)
      const prev = lastSeqRef.current.get(cursorKey) ?? 0
      if (seq <= prev) return
      lastSeqRef.current.set(cursorKey, seq)

      let event: Record<string, unknown>
      try {
        event = JSON.parse(payload.event || "{}") as Record<string, unknown>
      } catch {
        return
      }
      if (!event?.type) return

      // Ensure a streaming assistant message exists in the cache.
      // The initial placeholder (added by the mount effect or turn_started
      // listener) may have been overwritten by the message-load effect in
      // CronSessionViewer which does `setMessages(loaded)` — a full replace
      // that drops the placeholder.  Without it, `handleStreamEvent` finds
      // no assistant message to append text to and silently discards every
      // delta, producing the "no real-time output" symptom.
      const cached = sessionCacheRef.current.get(sessionId) || []
      const lastCached = cached[cached.length - 1]
      if (!lastCached || lastCached.role !== "assistant" || !lastCached.isStreaming) {
        isStreamingRef.current = true
        const withPlaceholder = appendStreamingPlaceholder(cached)
        sessionCacheRef.current.set(sessionId, withPlaceholder)
        setMessages(withPlaceholder)
      }

      handleStreamEvent(event, sessionId, {
        updateSessionMessages,
        deltaBuffersRef,
      })
    })
    return () => {
      unlisten()
      discardAllPendingStreamDeltas(deltaBuffersRef)
    }
  }, [sessionId, updateSessionMessages, appendStreamingPlaceholder, setMessages])

  // Listen for `chat:stream_end` — final DB reload.
  useEffect(() => {
    const unlisten = getTransport().listen(EVENT_CHAT_STREAM_END, (raw) => {
      const payload = raw as { sessionId?: string } | null
      if (!payload?.sessionId || payload.sessionId !== sessionId) return

      isStreamingRef.current = false
      discardPendingStreamDeltas(sessionId, deltaBuffersRef)
      lastSeqRef.current.clear()

      // Reload from DB for final reconciliation.
      void reloadAndMergeSessionMessages({
        sessionId,
        pageSize: PAGE_SIZE,
        sessionCacheRef,
        setMessages,
      })
    })
    return unlisten
  }, [sessionId, setMessages])
}
