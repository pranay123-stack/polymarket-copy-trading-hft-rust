import { useCallback, useEffect, useRef, useState } from 'react'

const TOKEN_KEY = 'copytrader.token'

export const getToken = () => localStorage.getItem(TOKEN_KEY) ?? ''
export const setToken = (t: string) => localStorage.setItem(TOKEN_KEY, t)

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = { 'Content-Type': 'application/json' }
  const t = getToken()
  if (t) headers.Authorization = `Bearer ${t}`
  const res = await fetch(path, { ...init, headers: { ...headers, ...(init?.headers ?? {}) } })
  if (!res.ok) {
    const body = await res.text()
    throw new Error(`${res.status}: ${body.slice(0, 200)}`)
  }
  return res.json() as Promise<T>
}

export const apiGet = <T,>(p: string) => request<T>(p)
export const apiPost = <T,>(p: string, body?: unknown) =>
  request<T>(p, { method: 'POST', body: JSON.stringify(body ?? {}) })
export const apiPatch = <T,>(p: string, body: unknown) =>
  request<T>(p, { method: 'PATCH', body: JSON.stringify(body) })
export const apiDelete = <T,>(p: string) => request<T>(p, { method: 'DELETE' })

/**
 * Polls an endpoint. The WebSocket carries events; this keeps tabular views
 * authoritative and is the resync path after a `lagged` notice.
 */
export function usePoll<T>(path: string, intervalMs = 2000, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const alive = useRef(true)

  const refresh = useCallback(async () => {
    try {
      const d = await apiGet<T>(path)
      if (alive.current) { setData(d); setError(null) }
    } catch (e) {
      if (alive.current) setError(e instanceof Error ? e.message : String(e))
    }
  }, [path])

  useEffect(() => {
    alive.current = true
    refresh()
    const id = setInterval(refresh, intervalMs)
    return () => { alive.current = false; clearInterval(id) }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, intervalMs, ...deps])

  return { data, error, refresh }
}
