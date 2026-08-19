import type { LatencyStage, OrderRow, PositionRow } from '../lib/types'
import { addr, ms, pnl, price, qty, signColor, statusColor, time, usd } from '../lib/format'
import { Empty, Panel, SideTag } from './primitives'

export function Positions({ rows }: { rows: PositionRow[] }) {
  return (
    <Panel title="Positions" right={<span className="text-term-dim">{rows.length}</span>} className="h-full">
      {rows.length === 0 ? (
        <Empty>Flat — no open positions.</Empty>
      ) : (
        <table className="w-full text-xs">
          <thead>
            <tr>
              <th className="th">Market</th><th className="th">Outcome</th>
              <th className="th text-right">Qty</th><th className="th text-right">Avg Entry</th>
              <th className="th text-right">Mark</th><th className="th text-right">Exposure</th>
              <th className="th text-right">Unrealised</th><th className="th text-right">Realised</th>
              <th className="th text-right">Total</th><th className="th text-right">Fees</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((p) => (
              <tr key={p.token_id} className="row">
                <td className="cell text-term-muted" title={p.market_id}>{addr(p.market_id)}</td>
                <td className="cell">{p.outcome || '—'}</td>
                <td className={`cell text-right ${Number(p.quantity) < 0 ? 'text-down' : ''}`}>{qty(p.quantity)}</td>
                <td className="cell text-right">{price(p.avg_entry)}</td>
                <td className="cell text-right">{p.mark_price ? price(p.mark_price) : <span className="text-warn" title="No mark available">—</span>}</td>
                <td className="cell text-right">{usd(p.exposure)}</td>
                <td className={`cell text-right ${signColor(p.unrealized_pnl)}`}>
                  {p.unrealized_pnl === null ? <span className="text-warn" title="Position is unmarked: genuinely unknown, not zero">unmarked</span> : pnl(p.unrealized_pnl)}
                </td>
                <td className={`cell text-right ${signColor(p.realized_pnl)}`}>{pnl(p.realized_pnl)}</td>
                <td className={`cell text-right ${signColor(p.total_pnl)}`}>{pnl(p.total_pnl)}</td>
                <td className="cell text-right text-term-dim">{usd(p.fees_paid)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  )
}

export function Orders({ rows }: { rows: OrderRow[] }) {
  return (
    <Panel title="Orders" right={<span className="text-term-dim">{rows.length}</span>} className="h-full">
      {rows.length === 0 ? (
        <Empty>No orders yet.</Empty>
      ) : (
        <table className="w-full text-xs">
          <thead>
            <tr>
              <th className="th">Created</th><th className="th">Order</th><th className="th">Market</th>
              <th className="th">Side</th><th className="th text-right">Qty</th>
              <th className="th text-right">Limit</th><th className="th text-right">Filled</th>
              <th className="th text-right">Avg Px</th><th className="th">Status</th>
              <th className="th">Mode</th><th className="th text-right">Ack</th>
              <th className="th text-right">Exec</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((o) => (
              <tr key={o.order_id} className="row">
                <td className="cell text-term-dim">{time(o.created_at)}</td>
                <td className="cell text-term-muted" title={`${o.order_id}\nvenue: ${o.venue_order_id ?? 'n/a'}`}>
                  {o.order_id.slice(0, 8)}
                </td>
                <td className="cell text-term-muted" title={o.market_id}>{addr(o.market_id)}</td>
                <td className="cell"><SideTag side={o.side} /></td>
                <td className="cell text-right">{qty(o.quantity)}</td>
                <td className="cell text-right">{price(o.limit_price)}</td>
                <td className="cell text-right">{qty(o.filled_qty)}</td>
                <td className="cell text-right">{o.avg_fill_price ? price(o.avg_fill_price) : '—'}</td>
                <td className={`cell ${statusColor(o.state)}`} title={o.reject_reason ?? ''}>
                  {o.state}
                </td>
                <td className="cell text-term-dim">{o.mode}</td>
                <td className="cell text-right text-term-muted">{ms(o.latency_ms.ack)}</td>
                <td className="cell text-right text-term-muted">{ms(o.latency_ms.execution)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  )
}

const STAGE_LABEL: Record<string, string> = {
  detection: 'Detection (venue publish → ingest)',
  strategy: 'Strategy (ingest → signal)',
  risk: 'Risk (signal → verdict)',
  submission: 'Submission (verdict → wire)',
  ack: 'Ack (wire → venue accept)',
  execution: 'Execution (wire → fill)',
  internal: 'Internal (ingest → wire) — ours to optimise',
  end_to_end: 'End-to-end (venue publish → fill)',
}

export function Latency({ stages }: { stages: LatencyStage[] }) {
  return (
    <Panel
      title="Latency"
      right={<span className="text-2xs text-term-dim">real measurements only — unmeasured stages are omitted</span>}
      className="h-full"
    >
      {stages.length === 0 ? (
        <Empty>No latency measured yet. Stages appear once a trade flows through.</Empty>
      ) : (
        <table className="w-full text-xs">
          <thead>
            <tr>
              <th className="th">Stage</th><th className="th text-right">n</th>
              <th className="th text-right">min</th><th className="th text-right">mean</th>
              <th className="th text-right">p50</th><th className="th text-right">p95</th>
              <th className="th text-right">p99</th><th className="th text-right">max</th>
            </tr>
          </thead>
          <tbody>
            {stages.map((s) => (
              <tr key={s.stage} className={`row ${s.stage === 'internal' ? 'bg-info/5' : ''}`}>
                <td className="cell">
                  <div>{s.stage}</div>
                  <div className="text-2xs text-term-dim">{STAGE_LABEL[s.stage] ?? ''}</div>
                </td>
                <td className="cell text-right text-term-muted">{s.count.toLocaleString()}</td>
                <td className="cell text-right text-term-muted">{ms(s.min_ms)}</td>
                <td className="cell text-right">{ms(s.mean_ms)}</td>
                <td className="cell text-right text-term-text">{ms(s.p50_ms)}</td>
                <td className="cell text-right text-warn">{ms(s.p95_ms)}</td>
                <td className="cell text-right text-warn">{ms(s.p99_ms)}</td>
                <td className="cell text-right text-down">{ms(s.max_ms)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  )
}
