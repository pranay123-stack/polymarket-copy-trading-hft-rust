import type { ReactNode } from 'react'
import type { HealthState } from '../lib/types'

export function Panel({
  title, right, children, className = '',
}: { title: string; right?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <div className={`panel flex flex-col min-h-0 ${className}`}>
      <div className="panel-title shrink-0">
        <span>{title}</span>
        {right}
      </div>
      <div className="flex-1 min-h-0 overflow-auto">{children}</div>
    </div>
  )
}

export function Stat({
  label, value, sub, tone = 'text-term-text', mono = true,
}: { label: string; value: ReactNode; sub?: ReactNode; tone?: string; mono?: boolean }) {
  return (
    <div className="panel px-3 py-2">
      <div className="text-2xs uppercase tracking-wider text-term-dim">{label}</div>
      <div className={`text-lg leading-tight ${tone} ${mono ? 'font-mono' : ''}`}>{value}</div>
      {sub !== undefined && <div className="text-2xs text-term-muted mt-0.5">{sub}</div>}
    </div>
  )
}

const DOT: Record<HealthState, string> = {
  HEALTHY: 'bg-up',
  DEGRADED: 'bg-warn',
  DOWN: 'bg-down',
}

export function Dot({ state }: { state: HealthState }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className={`w-1.5 h-1.5 rounded-full ${DOT[state]} ${state !== 'HEALTHY' ? 'animate-pulse' : ''}`} />
    </span>
  )
}

export function Tag({ children, tone }: { children: ReactNode; tone: string }) {
  return <span className={`tag ${tone}`}>{children}</span>
}

export function SideTag({ side }: { side: 'BUY' | 'SELL' }) {
  return (
    <Tag tone={side === 'BUY' ? 'bg-up/15 text-up' : 'bg-down/15 text-down'}>{side}</Tag>
  )
}

export function Empty({ children }: { children: ReactNode }) {
  return (
    <div className="flex items-center justify-center h-full min-h-[100px] text-term-dim text-xs px-4 text-center">
      {children}
    </div>
  )
}

/** A horizontal utilisation bar. Turns amber then red as a limit is approached. */
export function Meter({ value, label }: { value: number; label: string }) {
  const clamped = Math.max(0, Math.min(value, 1))
  const tone = value >= 1 ? 'bg-down' : value >= 0.75 ? 'bg-warn' : 'bg-info'
  return (
    <div className="space-y-1">
      <div className="flex justify-between text-2xs">
        <span className="text-term-muted">{label}</span>
        <span className={value >= 1 ? 'text-down' : value >= 0.75 ? 'text-warn' : 'text-term-muted'}>
          {(value * 100).toFixed(0)}%
        </span>
      </div>
      <div className="h-1 bg-term-border rounded overflow-hidden">
        <div className={`h-full ${tone} transition-all duration-300`} style={{ width: `${clamped * 100}%` }} />
      </div>
    </div>
  )
}
