import { useEffect, useMemo, useState } from 'react'
import { Header, type Tab } from './components/Header'
import { DetectionStats, Overview } from './components/Overview'
import { Latency, Orders, Positions } from './components/Tables'
import { Risk } from './components/Risk'
import { Wallets } from './components/Wallets'
import { usePoll } from './hooks/useApi'
import { useEventStream } from './hooks/useWebSocket'
import type {
  CopyRow, LatencyStage, OrderRow, PositionRow, RiskView, SourceTradeRow, StatusResponse, WalletRow,
} from './lib/types'

export default function App() {
  const [tab, setTab] = useState<Tab>('Overview')
  const [equity, setEquity] = useState<{ t: number; equity: number }[]>([])

  // The WebSocket drives responsiveness; polling keeps tables authoritative and is the
  // resync path after a `lagged` notice.
  const { status: wsStatus, events } = useEventStream()

  const { data: status, refresh: refreshStatus } = usePoll<StatusResponse>('/api/status', 1500)
  const { data: trades } = usePoll<{ copies: CopyRow[]; source_trades: SourceTradeRow[] }>('/api/trades', 1500)
  const { data: positions } = usePoll<{ positions: PositionRow[] }>('/api/positions', 2000)
  const { data: orders } = usePoll<{ orders: OrderRow[] }>('/api/orders', 2000)
  const { data: latency } = usePoll<{ stages: LatencyStage[] }>('/api/latency', 3000)
  const { data: risk, refresh: refreshRisk } = usePoll<RiskView>('/api/risk', 2000)
  const { data: wallets, refresh: refreshWallets } = usePoll<{ wallets: WalletRow[] }>('/api/target-wallets', 3000)

  // Equity series for the sparkline, sampled from the status poll.
  useEffect(() => {
    const e = status?.pnl.equity
    if (e === undefined) return
    const n = Number(e)
    if (!Number.isFinite(n)) return
    setEquity((prev) => {
      const next = [...prev, { t: Date.now(), equity: n }]
      return next.length > 240 ? next.slice(-240) : next
    })
  }, [status?.pnl.equity, status?.pnl.at])

  const copies = useMemo(() => trades?.copies ?? [], [trades])
  const sources = useMemo(() => trades?.source_trades ?? [], [trades])

  return (
    <div className="h-full flex flex-col bg-term-bg">
      <Header status={status} ws={wsStatus} tab={tab} onTab={setTab} />

      <main className="flex-1 min-h-0 p-2 overflow-hidden">
        {tab === 'Overview' && (
          <Overview
            status={status} copies={copies} sources={sources}
            latency={latency?.stages ?? []} equityHistory={equity}
          />
        )}
        {tab === 'Copy Trades' && (
          <Overview
            status={status} copies={copies} sources={sources}
            latency={latency?.stages ?? []} equityHistory={equity}
          />
        )}
        {tab === 'Positions' && <Positions rows={positions?.positions ?? []} />}
        {tab === 'Orders' && <Orders rows={orders?.orders ?? []} />}
        {tab === 'Wallets' && (
          <Wallets rows={wallets?.wallets ?? []} onChanged={refreshWallets} />
        )}
        {tab === 'Risk' && (
          <Risk risk={risk} events={events} onChanged={() => { refreshRisk(); refreshStatus() }} />
        )}
        {tab === 'Latency' && <Latency stages={latency?.stages ?? []} />}
      </main>

      <footer className="shrink-0 border-t border-term-border px-3 py-1.5 flex items-center justify-between">
        <DetectionStats status={status} />
        <span className="text-2xs text-term-dim">
          {status?.execution_adapter === 'paper'
            ? 'simulated execution against real order books'
            : status?.execution_adapter === 'live'
              ? 'live execution adapter'
              : '—'}
        </span>
      </footer>
    </div>
  )
}
