import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useI18n } from '../i18n'

export type Tone = 'emerald' | 'amber' | 'rose' | 'zinc' | 'signal'

const toneDot: Record<Tone, string> = {
  emerald: 'bg-c-em',
  signal: 'bg-signal',
  amber: 'bg-c-amb',
  rose: 'bg-c-rse',
  zinc: 'bg-c-zn',
}

const tonePill: Record<Tone, string> = {
  emerald: 'pill-emerald',
  signal: 'pill-signal',
  amber: 'pill-amber',
  rose: 'pill-rose',
  zinc: 'pill-zinc',
}

const toneBar: Record<Tone, string> = {
  emerald: 'bg-c-em',
  signal: 'bg-signal',
  amber: 'bg-c-amb',
  rose: 'bg-c-rse',
  zinc: 'bg-c-zn/60',
}

const toneStroke: Record<Tone, string> = {
  signal: 'var(--signal)',
  emerald: 'var(--c-emerald)',
  amber: 'var(--c-amber)',
  rose: 'var(--c-rose)',
  zinc: 'var(--c-zinc)',
}

export function Card({
  children,
  className = '',
  interactive = false,
}: {
  children: ReactNode
  className?: string
  interactive?: boolean
}) {
  return (
    <section className={`card ${interactive ? 'interactive' : ''} ${className}`}>
      {children}
    </section>
  )
}

export function SectionHeading({
  title,
  right,
  count,
}: {
  title: string
  right?: ReactNode
  count?: number
}) {
  return (
    <div className="mb-4 flex items-center gap-3">
      <h2 className="eyebrow text-ink-3">{title}</h2>
      {count != null && <span className="font-num text-[11px] text-ink-4">{count}</span>}
      <span className="hairline-x flex-1" />
      {right}
    </div>
  )
}

export function Dot({
  tone,
  className = '',
  pulse = false,
}: {
  tone: Tone
  className?: string
  pulse?: boolean
}) {
  return (
    <span
      className={`inline-block h-2 w-2 rounded-full ${toneDot[tone]} ${pulse ? 'pulse' : ''} ${className}`}
    />
  )
}

export function Pill({
  tone = 'zinc',
  title,
  children,
}: {
  tone?: Tone
  title?: string
  children: ReactNode
}) {
  return (
    <span title={title} className={`pill ${tonePill[tone]}`}>
      {children}
    </span>
  )
}

export function Bar({ pct, tone = 'signal' }: { pct: number | null; tone?: Tone }) {
  const w = pct === null ? 0 : Math.max(0, Math.min(100, pct))
  return (
    <div className="h-1 flex-1 overflow-hidden rounded-full bg-track">
      <div
        className={`h-full rounded-full transition-[width] duration-700 ease-out ${toneBar[tone]}`}
        style={{ width: `${w}%` }}
      />
    </div>
  )
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="py-8 text-center text-[13px] text-ink-4">{children}</p>
}

export function Muted({ children, className = '' }: { children: ReactNode; className?: string }) {
  return <span className={`text-ink-3 ${className}`}>{children}</span>
}

/** Rolling history sparkline — `domain: 'percent'` pins the scale
 *  to 0-100 (the absolute level stays honest); 'auto' fits min..max
 *  with padding so an unbounded metric like load still shows its
 *  shape. Light area + head dot. */
export function Sparkline({
  samples,
  width = 72,
  height = 20,
  tone = 'signal',
  domain = 'percent',
}: {
  samples: number[]
  width?: number
  height?: number
  tone?: Tone
  domain?: 'percent' | 'auto'
}) {
  const gid = `sg-${tone}`
  if (samples.length < 2) return <div style={{ width, height }} />
  let lo = 0
  let hi = domain === 'percent' ? 100 : Math.max(...samples)
  if (domain === 'auto') {
    lo = Math.min(...samples)
    const span = Math.max(hi - lo, hi * 0.2, 0.01)
    lo = Math.max(0, lo - span * 0.15)
    hi = hi + span * 0.15
  }
  const pts = samples.map((v, i) => {
    const x = (i / (samples.length - 1)) * width
    const y = height - 1.5 - ((v - lo) / (hi - lo || 1)) * (height - 4)
    return [x, Math.max(1, Math.min(height - 1.5, y))] as const
  })
  const line = pts.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(' ')
  const area = `0,${height} ${line} ${width},${height}`
  const [hx, hy] = pts[pts.length - 1]
  const stroke = toneStroke[tone]
  return (
    <svg width={width} height={height} className="shrink-0" aria-hidden>
      <defs>
        <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={stroke} stopOpacity="0.14" />
          <stop offset="100%" stopColor={stroke} stopOpacity="0" />
        </linearGradient>
      </defs>
      <polygon points={area} fill={`url(#${gid})`} />
      <polyline
        points={line}
        fill="none"
        stroke={stroke}
        strokeOpacity="0.85"
        strokeWidth="1.25"
        strokeLinejoin="round"
        strokeLinecap="round"
      />
      <circle cx={hx} cy={hy} r="1.75" fill={stroke} />
    </svg>
  )
}

/** Click-to-copy mono text — ids and names are meant to be grabbed. */
export function CopyText({ text, className = '' }: { text: string; className?: string }) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const timer = useRef<number | undefined>(undefined)
  useEffect(() => () => window.clearTimeout(timer.current), [])
  return (
    <button
      type="button"
      title={t('common.copy')}
      onClick={() => {
        void navigator.clipboard?.writeText(text)
        setCopied(true)
        window.clearTimeout(timer.current)
        timer.current = window.setTimeout(() => setCopied(false), 1200)
      }}
      className={`font-num inline-flex cursor-copy items-center gap-1 rounded px-1 -mx-1 text-left transition-colors duration-150 hover:bg-hover-2 hover:text-ink-1 ${copied ? 'text-signal-fg' : ''} ${className}`}
    >
      <span className="min-w-0 truncate">{copied ? t('common.copied') : text}</span>
    </button>
  )
}
