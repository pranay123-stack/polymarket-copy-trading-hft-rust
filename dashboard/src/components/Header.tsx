import type { StatusResponse } from '../lib/types'
import type { WsStatus } from '../hooks/useWebSocket'
import { uptime } from '../lib/format'
import { Dot } from './primitives'

const TABS = ['Overview', 'Copy Trades', 'Positions', 'Orders', 'Wallets', 'Risk', 'Latency'] as const
export type Tab = (typeof TABS)[number]

export function Header({
  status, ws, tab, onTab,
}: { status: StatusResponse | null; ws: WsStatus; tab: Tab; onTab: (t: Tab) => void }) {
  const mode = status?.mode ?? '—'
  const real = status?.real_money ?? false

  return (
    <header className="shrink-0 border-b border-term-border bg-term-panel">
      {/* A live book gets an unmissable banner — this is the one thing that must never
          be ambiguous on this screen. */}
      {real && (
        <div className="bg-down text-black text-center text-xs font-bold py-1 tracking-wider">
          ⚠ LIVE TRADING — REAL FUNDS AT RISK ⚠
        </div>
      )}
      {status?.kill_switch.engaged && (
        <div className="bg-warn text-black text-center text-xs font-bold py-1 tracking-wider">
          ⛔ KILL SWITCH ENGAGED — {status.kill_switch.reason ?? 'trading halted'}
        </div>
      )}

      <div className="flex items-center gap-4 px-4 h-12">
        <div className="flex items-center gap-2.5">
          <span className="text-term-text font-semibold tracking-tight">POLYMARKET</span>
          <span className="text-term-dim text-xs">COPY-TRADER</span>
        </div>

        <span
          className={`tag ${
            mode === 'LIVE' ? 'bg-down/20 text-down' :
            mode === 'REPLAY' ? 'bg-info/20 text-info' : 'bg-up/15 text-up'
          }`}
        >
          MODE: {mode}
        </span>

        {status?.storage.ephemeral && (
          <span className="tag bg-warn/15 text-warn" title="No database: durable audit and crash recovery are unavailable">
            EPHEMERAL
          </span>
        )}

        <nav className="flex gap-0.5 ml-2">
          {TABS.map((t) => (
            <button
              key={t}
              onClick={() => onTab(t)}
              className={`px-2.5 py-1 text-xs rounded transition-colors ${
                tab === t ? 'bg-term-hover text-term-text' : 'text-term-muted hover:text-term-text'
              }`}
            >
              {t}
            </button>
          ))}
        </nav>

        <div className="ml-auto flex items-center gap-4 text-2xs text-term-muted">
          <span className="flex items-center gap-1.5" title={`WebSocket ${ws}`}>
            <Dot state={ws === 'open' ? 'HEALTHY' : ws === 'connecting' ? 'DEGRADED' : 'DOWN'} />
            WS
          </span>
          {status?.health.components.map((c) => (
            <span key={c.name} className="flex items-center gap-1.5" title={c.detail}>
              <Dot state={c.state} />
              {c.name.replace(/_/g, ' ')}
            </span>
          ))}
          {status && <span className="text-term-dim">up {uptime(status.uptime_seconds)}</span>}
        </div>
      </div>
    </header>
  )
}
