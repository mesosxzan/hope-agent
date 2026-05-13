import { describe, expect, test, vi } from "vitest"
import type { Message, SessionMessage } from "@/types/chat"
import type { Transport } from "@/lib/transport"
import { setTransport } from "@/lib/transport-provider"
import { reloadAndMergeSessionMessages } from "./chatUtils"

function sessionMessage(patch: Partial<SessionMessage>): SessionMessage {
  return {
    id: 1,
    sessionId: "s1",
    role: "assistant",
    content: "",
    timestamp: "2026-05-12T00:00:00.000Z",
    ...patch,
  }
}

describe("reloadAndMergeSessionMessages", () => {
  test("merges against latest cache after async DB load resolves", async () => {
    let resolveLoad:
      | ((value: [SessionMessage[], number, boolean]) => void)
      | undefined
    const transport = {
      call: vi.fn(() => new Promise<[SessionMessage[], number, boolean]>((resolve) => {
        resolveLoad = resolve
      })),
    } as unknown as Transport
    setTransport(transport)

    const sessionCacheRef = {
      current: new Map<string, Message[]>([
        [
          "s1",
          [
            {
              role: "assistant",
              content: "failed partial",
              timestamp: "2026-05-12T00:00:00.000Z",
              dbId: 1,
            },
            {
              role: "event",
              content: "failed",
              timestamp: "2026-05-12T00:00:01.000Z",
              dbId: 2,
            },
          ],
        ],
      ]),
    }
    const setMessages = vi.fn()

    const reload = reloadAndMergeSessionMessages({
      sessionId: "s1",
      pageSize: 50,
      sessionCacheRef,
      setMessages,
    })

    sessionCacheRef.current.set("s1", [
      ...sessionCacheRef.current.get("s1")!,
      {
        role: "user",
        content: "继续",
        timestamp: "2026-05-12T00:00:02.000Z",
        _clientId: "user-next",
      },
      {
        role: "assistant",
        content: "",
        timestamp: "2026-05-12T00:00:03.000Z",
        _clientId: "assistant-next",
      },
    ])

    resolveLoad?.([
      [
        sessionMessage({ id: 1, content: "failed partial" }),
        sessionMessage({
          id: 2,
          role: "event",
          content: "failed",
          timestamp: "2026-05-12T00:00:01.000Z",
        }),
      ],
      2,
      false,
    ])
    await reload

    const merged = sessionCacheRef.current.get("s1")
    expect(merged?.map((msg) => msg.role)).toEqual([
      "assistant",
      "event",
      "user",
      "assistant",
    ])
    expect(merged?.at(-1)).toMatchObject({
      role: "assistant",
      _clientId: "assistant-next",
    })
  })

  test("does not re-append placeholders already finalized by DB rows", async () => {
    let resolveLoad:
      | ((value: [SessionMessage[], number, boolean]) => void)
      | undefined
    const transport = {
      call: vi.fn(() => new Promise<[SessionMessage[], number, boolean]>((resolve) => {
        resolveLoad = resolve
      })),
    } as unknown as Transport
    setTransport(transport)

    const sessionCacheRef = {
      current: new Map<string, Message[]>([
        [
          "s1",
          [
            {
              role: "assistant",
              content: "failed partial",
              timestamp: "2026-05-12T00:00:00.000Z",
              dbId: 1,
            },
            {
              role: "event",
              content: "failed",
              timestamp: "2026-05-12T00:00:01.000Z",
              dbId: 2,
            },
          ],
        ],
      ]),
    }
    const setMessages = vi.fn()

    const reload = reloadAndMergeSessionMessages({
      sessionId: "s1",
      pageSize: 50,
      sessionCacheRef,
      setMessages,
    })

    sessionCacheRef.current.set("s1", [
      ...sessionCacheRef.current.get("s1")!,
      {
        role: "user",
        content: "继续",
        timestamp: "2026-05-12T00:00:02.000Z",
        _clientId: "user-next",
      },
      {
        role: "assistant",
        content: "",
        timestamp: "2026-05-12T00:00:03.000Z",
        _clientId: "assistant-next",
      },
    ])

    resolveLoad?.([
      [
        sessionMessage({ id: 1, content: "failed partial" }),
        sessionMessage({
          id: 2,
          role: "event",
          content: "failed",
          timestamp: "2026-05-12T00:00:01.000Z",
        }),
        sessionMessage({
          id: 3,
          role: "user",
          content: "继续",
          timestamp: "2026-05-12T00:00:02.000Z",
        }),
        sessionMessage({
          id: 4,
          role: "assistant",
          content: "new answer",
          timestamp: "2026-05-12T00:00:03.000Z",
        }),
      ],
      4,
      false,
    ])
    await reload

    const merged = sessionCacheRef.current.get("s1")
    expect(merged?.map((msg) => msg.dbId)).toEqual([1, 2, 3, 4])
    expect(merged?.find((msg) => msg.dbId === 3)).toMatchObject({
      role: "user",
      _clientId: "user-next",
    })
    expect(merged?.find((msg) => msg.dbId === 4)).toMatchObject({
      role: "assistant",
      content: "new answer",
      _clientId: "assistant-next",
    })
  })

  test("preserves dbId-less messages with identical fallback fields", async () => {
    let resolveLoad:
      | ((value: [SessionMessage[], number, boolean]) => void)
      | undefined
    const transport = {
      call: vi.fn(() => new Promise<[SessionMessage[], number, boolean]>((resolve) => {
        resolveLoad = resolve
      })),
    } as unknown as Transport
    setTransport(transport)

    const localEvent = {
      role: "event",
      content: "",
      timestamp: "2026-05-12T00:00:00.000Z",
    } satisfies Message
    const persistedMessage = {
      role: "assistant",
      content: "persisted",
      timestamp: "2026-05-12T00:00:01.000Z",
      dbId: 1,
    } satisfies Message
    const sessionCacheRef = {
      current: new Map<string, Message[]>([["s1", [localEvent, persistedMessage]]]),
    }
    const setMessages = vi.fn()

    const reload = reloadAndMergeSessionMessages({
      sessionId: "s1",
      pageSize: 50,
      sessionCacheRef,
      setMessages,
    })

    sessionCacheRef.current.set("s1", [
      localEvent,
      persistedMessage,
      {
        role: "event",
        content: "",
        timestamp: "2026-05-12T00:00:00.000Z",
      },
    ])

    resolveLoad?.([[sessionMessage({ id: 1, content: "persisted" })], 1, false])
    await reload

    const merged = sessionCacheRef.current.get("s1")
    expect(merged).toHaveLength(3)
    expect(merged?.filter((msg) => msg.role === "event" && !msg.dbId)).toHaveLength(2)
  })
})
