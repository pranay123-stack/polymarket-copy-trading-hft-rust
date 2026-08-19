import { LineChart, Line, ResponsiveContainer, YAxis, Tooltip } from 'recharts'
import type { CopyRow, LatencyStage, SourceTradeRow, StatusResponse } from '../lib/types'
import { addr, ago, ms, pnl, price, qty, signColor, statusColor, time, usd } from '../lib/format'
import { Empty, Panel, SideTag, Stat } from './primitives'

export function Overview({
  status, copies, sources, latency, equityHistory,
}: {
  status: StatusResponse | null
  copies: CopyRow[]
  sources: SourceTradeRow[]
  latency: LatencyStage[]
  equityHistory: { t: number; equity: number }[]
}) {
  const p = status?.pnl
  const e2e = latency.find((l) => l.stage === 'end_to_end')
  const det = latency.find((l) => l.stage === 'detection')
  const internal = latency.find((l) => l.stage === 'internal')

  return (
    <div className="grid gap-2 h-full min-h-0" style={{ gridTemplateRows: 'auto auto 1fr' }}>
      <div className="grid grid-cols-2 md:grid-cols-4 lg:grid-cols-7 gap-2">
        <Stat label="Equity" value={usd(p?.equity)} sub={`cash ${usd(p?.cash)}`} />
        <Stat
          label="Total PnL"
          value={pnl(
            p ? Number(p.realized_pnl) + Number(p.unrealized_pnl ?? 0) : null,
          )}
          tone={signColor(p ? Number(p.realized_pnl) + Number(p.unrealized_pnl ?? 0) : null)}
          sub={p?.unrealized_pnl === null ? 'unrealised unknown (unmarked)' : `realised ${pnl(p?.realized_pnl)}`}
        />
        <Stat label="Daily PnL" value={pnl(p?.daily_pnl)} tone={signColor(p?.daily_pnl)} sub={`fees ${usd(p?.fees_paid)}`} />
        <Stat label="Exposure" value={usd(p?.gross_exposure)} sub={`${p?.active_positions ?? 0} positions`} />
        <Stat label="Open Orders" value={p?.open_orders ?? 0} sub={`${status?.tracker.wallets ?? 0} wallets tracked`} />
        <Stat
          label="End-to-End p50"
          value={ms(e2e?.p50_ms ?? null)}
          sub={e2e ? `p99 ${ms(e2e.p99_ms)} · n=${e2e.count}` : 'not yet measured'}
        />
        <Stat
          label="Our Latency p50"
          value={ms(internal?.p50_ms ?? null)}
          tone="text-info"
          sub={det ? `venue publish ${ms(det.p50_ms)}` : 'ingest → wire'}
        />
      </div>

      <div className="panel p-2 h-28">
        <div className="text-2xs uppercase tracking-wider text-term-dim mb-1 px-1">Equity</div>
        {equityHistory.length > 1 ? (
          <ResponsiveContainer width="100%" height="85%">
            <LineChart data={equityHistory}>
              <YAxis domain={['dataMin', 'dataMax']} hide />
              <Tooltip
                contentStyle={{ background: '#111621', border: '1px solid #1e2635', fontSize: 11 }}
                labelFormatter={() => ''}
                formatter={(v: number) => [usd(v), 'equity']}
              />
              <Line
                type="monotone" dataKey="equity" stroke="#3d8bfd" strokeWidth={1.5}
                dot={false} isAnimationActive={false}
              />
            </LineChart>
          </ResponsiveContainer>
        ) : (
          <Empty>Collecting equity samples…</Empty>
        )}
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-2 min-h-0">
        <Panel title="Copied Trades" right={<span className="text-term-dim">{copies.length}</span>}>
          {copies.length === 0 ? (
            <Empty>No copies yet. A tracked wallet must trade first.</Empty>
          ) : (
            <table className="w-full text-xs">
              <thead>
                <tr>
                  <th className="th">Time</th><th className="th">Wallet</th><th className="th">Market</th>
                  <th className="th">Side</th><th className="th text-right">Source</th>
                  <th className="th text-right">Copied</th><th className="th text-right">Px</th>
                  <th className="th text-right">Slip</th><th className="th text-right">E2E</th>
                  <th className="th">Status</th>
                </tr>
              </thead>
              <tbody>
                {copies.slice(0, 60).map((c) => (
                  <tr key={c.correlation_id} className="row">
                    <td className="cell text-term-dim">{time(c.at)}</td>
                    <td className="cell" title={c.wallet}>{c.wallet_nickname}</td>
                    <td className="cell max-w-[180px] truncate" title={c.market_title}>
                      {c.market_title} <span className="text-term-dim">· {c.outcome}</span>
                    </td>
                    <td className="cell"><SideTag side={c.side} /></td>
                    <td className="cell text-right text-term-muted">{usd(c.source_notional, 0)}</td>
                    <td className="cell text-right">{usd(c.copy_notional, 0)}</td>
                    <td className="cell text-right">
                      {price(c.source_price)}
                      {c.copy_price && <span className="text-term-dim"> → {price(c.copy_price)}</span>}
                    </td>
                    <td className={`cell text-right ${c.slippage_bps && c.slippage_bps > 0 ? 'text-down' : 'text-up'}`}>
                      {c.slippage_bps === null ? '—' : `${c.slippage_bps}bp`}
                    </td>
                    <td className="cell text-right text-term-muted">{ms(c.end_to_end_latency_ms)}</td>
                    <td className={`cell ${statusColor(c.status)}`}>{c.status}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Panel>

        <Panel title="Source Trades (detected)" right={<span className="text-term-dim">{sources.length}</span>}>
          {sources.length === 0 ? (
            <Empty>Watching the firehose for tracked wallets…</Empty>
          ) : (
            <table className="w-full text-xs">
              <thead>
                <tr>
                  <th className="th">Time</th><th className="th">Trader</th><th className="th">Market</th>
                  <th className="th">Side</th><th className="th text-right">Px</th>
                  <th className="th text-right">Qty</th><th className="th text-right">Notional</th>
                  <th className="th">Feed</th><th className="th text-right">Lag</th>
                </tr>
              </thead>
              <tbody>
                {sources.slice(0, 60).map((s) => {
                  const lag = (new Date(s.detected_ts).getTime() - new Date(s.source_ts).getTime()) / 1000
                  return (
                    <tr key={s.event_id} className="row">
                      <td className="cell text-term-dim">{time(s.detected_ts)}</td>
                      <td className="cell" title={s.trader}>{addr(s.trader)}</td>
                      <td className="cell max-w-[180px] truncate" title={s.market_title}>
                        {s.market_title} <span className="text-term-dim">· {s.outcome}</span>
                      </td>
                      <td className="cell"><SideTag side={s.side} /></td>
                      <td className="cell text-right">{price(s.price)}</td>
                      <td className="cell text-right text-term-muted">{qty(s.quantity)}</td>
                      <td className="cell text-right">{usd(s.notional, 0)}</td>
                      <td className="cell">
                        <span className={`tag ${s.source === 'demo' ? 'bg-warn/15 text-warn' : 'bg-term-border text-term-muted'}`}>
                          {s.source}
                        </span>
                      </td>
                      <td className="cell text-right text-term-dim">{ms(lag * 1000)}</td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          )}
        </Panel>
      </div>
    </div>
  )
}

export function DetectionStats({ status }: { status: StatusResponse | null }) {
  if (!status) return null
  const t = status.tracker
  return (
    <div className="flex gap-4 text-2xs text-term-muted px-1">
      <span>frames <span className="text-term-text">{t.frames_examined.toLocaleString()}</span></span>
      <span>matched <span className="text-term-text">{t.wallet_matches.toLocaleString()}</span></span>
      <span>actionable <span className="text-up">{t.actionable.toLocaleString()}</span></span>
      <span title="Re-delivered fills recognised by content+occurrence hashing">
        dupes suppressed <span className="text-warn">{t.duplicates_suppressed.toLocaleString()}</span>
      </span>
      <span>dedup index <span className="text-term-text">{t.dedup_contents.toLocaleString()}</span></span>
      <span>seen {ago(status.pnl.at)} ago</span>
    </div>
  )
}
