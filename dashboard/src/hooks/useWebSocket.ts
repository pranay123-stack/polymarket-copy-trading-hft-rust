import { useEffect, useRef, useState } from 'react'
import type { WsEnvelope } from '../lib/types'

export type WsStatus = 'connecting' | 'open' | 'closed'

/**
 * Live event stream from the backend.
 *
 * Reconnects with backoff. A `lagged` frame means the backend dropped events for this
 * client rather than slowing the trading pipeline — the UI treats it as a signal to
 * resync from REST, not as an error.
 */
export function useEventStream(onEvent?: (e: WsEnvelope) => void) {
  const [status, setStatus] = useState<WsStatus>('connecting')
  const [events, setEvents] = useState<WsEnvelope[]>([])
  const [lagged, setLagged] = useState(0)
  const cb = useRef(onEvent)
  cb.current = onEvent

  useEffect(() => {
    let ws: WebSocket | null = null
    let timer: ReturnType<typeof setTimeout> | null = null
    let attempt = 0
    let closed = false

    const connect = () => {
      if (closed) return
      setStatus('connecting')
      const proto = location.protocol === 'https:' ? 'wss' : 'ws'
      ws = new WebSocket(`${proto}://${location.host}/ws`)

      ws.onopen = () => { attempt = 0; setStatus('open') }
      ws.onclose = () => {
        setStatus('closed')
        if (closed) return
        // Jittered backoff, capped, so a backend restart is not hammered.
        const delay = Math.min(500 * 2 ** attempt++, 15000) * (0.5 + Math.random() / 2)
        timer = setTimeout(connect, delay)
      }
      ws.onerror = () => ws?.close()
      ws.onmessage = (m) => {
        try {
          const env = JSON.parse(m.data as string) as WsEnvelope
          if (env.kind === 'lagged') {
            setLagged((n) => n + 1)
            return
          }
          cb.current?.(env)
          setEvents((prev) => [env, ...prev].slice(0, 300))
        } catch { /* ignore malformed frames */ }
      }
    }

    connect()
    return () => {
      closed = true
      if (timer) clearTimeout(timer)
      ws?.close()
    }
  }, [])

  return { status, events, lagged }
}
