import { useState } from 'react'
import type { WalletRow } from '../lib/types'
import { addr, pnl, signColor, usd } from '../lib/format'
import { apiDelete, apiPatch, apiPost } from '../hooks/useApi'
import { Empty, Panel } from './primitives'

export function Wallets({ rows, onChanged }: { rows: WalletRow[]; onChanged: () => void }) {
  const [address, setAddress] = useState('')
  const [nickname, setNickname] = useState('')
  const [ratio, setRatio] = useState('0.25')
  const [maxTrade, setMaxTrade] = useState('100')
  const [err, setErr] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const wrap = async (fn: () => Promise<unknown>) => {
    setBusy(true); setErr(null)
    try { await fn(); onChanged() }
    catch (e) { setErr(e instanceof Error ? e.message : String(e)) }
    finally { setBusy(false) }
  }

  const add = () => wrap(async () => {
    await apiPost('/api/target-wallets', {
      address: address.trim(),
      nickname: nickname.trim() || undefined,
      copy_ratio: Number(ratio),
      max_trade_usd: Number(maxTrade),
    })
    setAddress(''); setNickname('')
  })

  return (
    <div className="grid grid-cols-1 lg:grid-cols-[1fr_320px] gap-2 h-full min-h-0">
      <Panel title="Target Wallets" right={<span className="text-term-dim">{rows.length}</span>}>
        {rows.length === 0 ? (
          <Empty>No target wallets configured. Add one to start copying.</Empty>
        ) : (
          <table className="w-full text-xs">
            <thead>
              <tr>
                <th className="th">Name</th><th className="th">Address</th>
                <th className="th">Sizing</th><th className="th text-right">Max Trade</th>
                <th className="th text-right">Max Exposure</th><th className="th text-right">Min Source</th>
                <th className="th text-right">PnL</th><th className="th">Status</th><th className="th"></th>
              </tr>
            </thead>
            <tbody>
              {rows.map((w) => (
                <tr key={w.address} className="row">
                  <td className="cell">{w.nickname}</td>
                  <td className="cell text-term-muted" title={w.address}>{addr(w.address)}</td>
                  <td className="cell text-term-muted">{describeSizing(w.sizing)}</td>
                  <td className="cell text-right">{usd(w.max_trade_usd, 0)}</td>
                  <td className="cell text-right">{usd(w.max_exposure_usd, 0)}</td>
                  <td className="cell text-right text-term-dim">{usd(w.min_source_notional_usd, 0)}</td>
                  <td className={`cell text-right ${signColor(w.pnl)}`}>{pnl(w.pnl)}</td>
                  <td className="cell">
                    <button
                      disabled={busy}
                      onClick={() => wrap(() => apiPatch(`/api/target-wallets/${w.address}`, { enabled: !w.enabled }))}
                      className={`tag ${w.enabled ? 'bg-up/15 text-up' : 'bg-term-border text-term-muted'}`}
                    >
                      {w.enabled ? 'ENABLED' : 'DISABLED'}
                    </button>
                  </td>
                  <td className="cell text-right">
                    <button
                      disabled={busy}
                      onClick={() => wrap(() => apiDelete(`/api/target-wallets/${w.address}`))}
                      className="text-term-dim hover:text-down transition-colors"
                      title="Remove"
                    >
                      ✕
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <div className="panel p-3 h-fit space-y-2">
        <div className="text-2xs uppercase tracking-wider text-term-dim">Add Target Wallet</div>
        <Field label="Address" value={address} onChange={setAddress} placeholder="0x…" mono />
        <Field label="Nickname" value={nickname} onChange={setNickname} placeholder="Whale" />
        <Field label="Copy ratio" value={ratio} onChange={setRatio} placeholder="0.25" />
        <Field label="Max trade (USD)" value={maxTrade} onChange={setMaxTrade} placeholder="100" />
        <button
          disabled={busy || !address.trim()}
          onClick={add}
          className="w-full py-1.5 bg-info/80 hover:bg-info text-black font-medium text-xs rounded disabled:opacity-40 transition-colors"
        >
          Add Wallet
        </button>
        {err && <div className="text-2xs text-down break-all">{err}</div>}
        <div className="text-2xs text-term-dim pt-1 border-t border-term-border">
          Per-wallet limits are additionally capped by the global risk limits; a wallet
          can never be given a larger budget than the system allows.
        </div>
      </div>
    </div>
  )
}

function Field({
  label, value, onChange, placeholder, mono,
}: { label: string; value: string; onChange: (v: string) => void; placeholder?: string; mono?: boolean }) {
  return (
    <label className="block">
      <span className="text-2xs text-term-muted">{label}</span>
      <input
        value={value} placeholder={placeholder} onChange={(e) => onChange(e.target.value)}
        className={`w-full bg-term-bg border border-term-border rounded px-2 py-1 text-xs mt-0.5
                    outline-none focus:border-info ${mono ? 'font-mono' : ''}`}
      />
    </label>
  )
}

function describeSizing(s: Record<string, unknown>): string {
  const mode = s.mode as string | undefined
  if (mode === 'fixed_ratio') return `ratio ${s.ratio}`
  if (mode === 'fixed_usd') return `fixed $${s.amount}`
  if (mode === 'portfolio_percent') return `${Number(s.pct) * 100}% equity`
  if (mode === 'risk_adjusted') return `risk-adj ${s.base_ratio}`
  return mode ?? '—'
}
