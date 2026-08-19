import { useState } from 'react'
import type { RiskView, WsEnvelope } from '../lib/types'
import { pnl, pct, time, usd } from '../lib/format'
import { apiPost, getToken, setToken } from '../hooks/useApi'
import { Empty, Meter, Panel } from './primitives'

export function Risk({
  risk, events, onChanged,
}: { risk: RiskView | null; events: WsEnvelope[]; onChanged: () => void }) {
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  const [token, setTok] = useState(getToken())

  const act = async (path: string, body?: unknown) => {
    setBusy(true); setErr(null)
    try { await apiPost(path, body); onChanged() }
    catch (e) { setErr(e instanceof Error ? e.message : String(e)) }
    finally { setBusy(false) }
  }

  const engaged = risk?.kill_switch.engaged ?? false
  const riskEvents = events.filter(
    (e) => e.kind === 'order_risk_rejected' || e.kind === 'risk_limit_breached'
      || e.kind === 'kill_switch_activated' || e.kind === 'reconciliation_mismatch',
  )

  return (
    <div className="grid grid-cols-1 lg:grid-cols-3 gap-2 h-full min-h-0">
      <div className="space-y-2">
        <div className={`panel p-4 ${engaged ? 'border-warn' : ''}`}>
          <div className="text-2xs uppercase tracking-wider text-term-dim mb-2">Emergency Stop</div>
          <div className={`text-2xl mb-1 ${engaged ? 'text-warn' : 'text-up'}`}>
            {engaged ? 'ENGAGED' : 'ARMED'}
          </div>
          <div className="text-2xs text-term-muted mb-3">
            {engaged
              ? `${risk?.kill_switch.reason ?? ''} — by ${risk?.kill_switch.engaged_by ?? '?'}`
              : 'Trading permitted. The backend enforces this, not the UI.'}
          </div>

          {!engaged ? (
            <button
              disabled={busy}
              onClick={() => act('/api/risk/kill-switch', { reason: 'manual stop from dashboard', cancel_open_orders: true })}
              className="w-full py-2 bg-down/90 hover:bg-down text-black font-bold text-xs rounded disabled:opacity-50 transition-colors"
            >
              ⛔ STOP TRADING &amp; CANCEL ORDERS
            </button>
          ) : (
            <button
              disabled={busy}
              onClick={() => act('/api/risk/kill-switch/reset')}
              className="w-full py-2 bg-up/80 hover:bg-up text-black font-bold text-xs rounded disabled:opacity-50 transition-colors"
            >
              RESUME TRADING
            </button>
          )}
          {risk && (
            <div className="text-2xs text-term-dim mt-2">
              activations this session: {risk.kill_switch.activations}
            </div>
          )}
          {err && <div className="text-2xs text-down mt-2">{err}</div>}
        </div>

        <div className="panel p-3">
          <div className="text-2xs uppercase tracking-wider text-term-dim mb-2">Limit Utilisation</div>
          {risk ? (
            <div className="space-y-2.5">
              <Meter label="Daily loss budget" value={risk.utilisation.daily_loss} />
              <Meter label="Portfolio exposure" value={risk.utilisation.exposure} />
              <Meter label="Open order slots" value={risk.utilisation.open_orders} />
            </div>
          ) : <Empty>—</Empty>}
        </div>

        <div className="panel p-3 space-y-1 text-xs">
          <div className="text-2xs uppercase tracking-wider text-term-dim mb-1">Current</div>
          {risk && (
            <>
              <Row k="Daily PnL" v={pnl(risk.current.daily_pnl)} />
              <Row k="Gross exposure" v={usd(risk.current.gross_exposure)} />
              <Row k="Open orders" v={String(risk.current.open_orders)} />
              <Row k="Equity" v={usd(risk.current.equity)} />
              <Row k="Drawdown" v={pct(risk.current.drawdown_pct)} />
            </>
          )}
        </div>

        <div className="panel p-3">
          <div className="text-2xs uppercase tracking-wider text-term-dim mb-2">API Token</div>
          <div className="flex gap-1.5">
            <input
              type="password" value={token} placeholder="required for mutating actions"
              onChange={(e) => setTok(e.target.value)}
              className="flex-1 bg-term-bg border border-term-border rounded px-2 py-1 text-xs outline-none focus:border-info"
            />
            <button
              onClick={() => { setToken(token); setErr(null) }}
              className="px-3 bg-term-border hover:bg-term-hover rounded text-xs transition-colors"
            >
              Save
            </button>
          </div>
        </div>
      </div>

      <Panel title="Rejections by Reason" className="min-h-0">
        {!risk || Object.keys(risk.rejections).length === 0 ? (
          <Empty>No orders have been refused.</Empty>
        ) : (
          <table className="w-full text-xs">
            <thead><tr><th className="th">Reason</th><th className="th text-right">Count</th></tr></thead>
            <tbody>
              {Object.entries(risk.rejections)
                .sort((a, b) => b[1] - a[1])
                .map(([code, n]) => (
                  <tr key={code} className="row">
                    <td className="cell text-down">{code.replace(/_/g, ' ')}</td>
                    <td className="cell text-right">{n}</td>
                  </tr>
                ))}
              <tr className="row border-t-2 border-term-border">
                <td className="cell text-term-muted">total</td>
                <td className="cell text-right">{risk.rejections_total}</td>
              </tr>
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="Risk Events" right={<span className="text-term-dim">{riskEvents.length}</span>} className="min-h-0">
        {riskEvents.length === 0 ? (
          <Empty>No risk events.</Empty>
        ) : (
          <div className="divide-y divide-term-border/60">
            {riskEvents.slice(0, 80).map((e, i) => (
              <div key={i} className="px-3 py-1.5 text-2xs">
                <div className="flex justify-between">
                  <span className={e.critical ? 'text-down' : 'text-warn'}>{e.kind.replace(/_/g, ' ')}</span>
                  <span className="text-term-dim">{time(e.at)}</span>
                </div>
                <div className="text-term-muted mt-0.5 break-all line-clamp-2">
                  {JSON.stringify(e.payload).slice(0, 220)}
                </div>
              </div>
            ))}
          </div>
        )}
      </Panel>
    </div>
  )
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between">
      <span className="text-term-muted">{k}</span>
      <span>{v}</span>
    </div>
  )
}
