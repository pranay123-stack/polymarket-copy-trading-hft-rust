/** Display helpers. All values arrive as decimal strings from Rust and are never
 *  re-parsed into floats for arithmetic — only for presentation. */

export const num = (v: string | number | null | undefined): number | null => {
  if (v === null || v === undefined || v === '') return null
  const n = typeof v === 'number' ? v : Number(v)
  return Number.isFinite(n) ? n : null
}

export const usd = (v: string | number | null | undefined, dp = 2): string => {
  const n = num(v)
  if (n === null) return '—'
  const sign = n < 0 ? '-' : ''
  return `${sign}$${Math.abs(n).toLocaleString('en-US', {
    minimumFractionDigits: dp,
    maximumFractionDigits: dp,
  })}`
}

/** Signed PnL with an explicit + so direction is unmistakable at a glance. */
export const pnl = (v: string | number | null | undefined): string => {
  const n = num(v)
  if (n === null) return '—'
  return `${n >= 0 ? '+' : '-'}$${Math.abs(n).toLocaleString('en-US', {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })}`
}

export const qty = (v: string | number | null | undefined): string => {
  const n = num(v)
  if (n === null) return '—'
  return n.toLocaleString('en-US', { maximumFractionDigits: 4 })
}

export const price = (v: string | number | null | undefined): string => {
  const n = num(v)
  return n === null ? '—' : n.toFixed(n < 0.1 ? 4 : 3)
}

export const pct = (v: string | number | null | undefined, dp = 1): string => {
  const n = num(v)
  return n === null ? '—' : `${(n * 100).toFixed(dp)}%`
}

export const ms = (v: number | null | undefined): string => {
  if (v === null || v === undefined) return '—'
  if (v < 1) return `${(v * 1000).toFixed(0)}µs`
  if (v < 1000) return `${v.toFixed(1)}ms`
  return `${(v / 1000).toFixed(2)}s`
}

export const addr = (a: string): string =>
  a.length > 12 ? `${a.slice(0, 6)}…${a.slice(-4)}` : a

export const time = (iso: string): string => {
  try {
    return new Date(iso).toLocaleTimeString('en-GB', { hour12: false })
  } catch {
    return '—'
  }
}

export const ago = (iso: string): string => {
  const d = (Date.now() - new Date(iso).getTime()) / 1000
  if (!Number.isFinite(d)) return '—'
  if (d < 60) return `${Math.floor(d)}s`
  if (d < 3600) return `${Math.floor(d / 60)}m`
  return `${Math.floor(d / 3600)}h`
}

export const uptime = (s: number): string => {
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  return h > 0 ? `${h}h ${m}m` : `${m}m ${s % 60}s`
}

/** Semantic colour for a signed value. */
export const signColor = (v: string | number | null | undefined): string => {
  const n = num(v)
  if (n === null || n === 0) return 'text-term-muted'
  return n > 0 ? 'text-up' : 'text-down'
}

/** Order/copy status → colour. Unknown states must stand out, not blend in. */
export const statusColor = (s: string): string => {
  if (s.startsWith('REJECTED')) return 'text-down'
  switch (s) {
    case 'FILLED': return 'text-up'
    case 'PARTIALLY_FILLED': return 'text-warn'
    case 'CANCELLED': return 'text-term-muted'
    case 'FAILED': return 'text-down'
    case 'UNKNOWN': return 'text-warn'
    case 'ACKNOWLEDGED':
    case 'SUBMITTED': return 'text-info'
    default: return 'text-term-text'
  }
}
