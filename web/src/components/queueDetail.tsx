import { useEffect, useRef, useState } from 'react'
import type { EntryDetail, ExitRecord, StatusReport } from '../types'
import { mibToGib } from '../fmt'
import { fetchLogs, fetchScript } from '../api'
import { Bar, CopyText, Dot, Empty, Muted, Pill } from './ui'
import type { Tone } from './ui'
import { useI18n } from '../i18n'

function barTone(pct: number): Tone {
  return pct > 95 ? 'rose' : pct > 80 ? 'amber' : 'signal'
}

function GaugeRow({ label, pct, display }: { label: string; pct: number | null; display: string }) {
  return (
    <div className="flex items-center gap-3">
      <span className="w-16 shrink-0 text-[11px] text-ink-4">{label}</span>
      <Bar pct={pct} tone={pct != null ? barTone(pct) : 'zinc'} />
      <span className="num-display w-28 shrink-0 text-right text-base text-ink-1">{display}</span>
    </div>
  )
}

export function ResourceBlock({ report, cores }: { report: StatusReport; cores?: number }) {
  const { t } = useI18n()
  const r = report.resources
  if (!r) return null
  const ramPct =
    r.ram_total_mb != null && r.ram_free_mb != null
      ? ((r.ram_total_mb - r.ram_free_mb) / r.ram_total_mb) * 100
      : null
  return (
    <div className="mt-3 space-y-2.5">
      {r.gpus.map((g) => {
        const vramUsed =
          g.mem_total_mb != null && g.mem_free_mb != null ? g.mem_total_mb - g.mem_free_mb : null
        const vramPct =
          vramUsed != null && g.mem_total_mb ? (vramUsed / g.mem_total_mb) * 100 : null
        return (
          <div key={g.uuid} className="space-y-1.5">
            <div className="flex items-baseline gap-2 text-[11px]">
              <span className="font-num text-ink-3">GPU{g.index}</span>
              <span className="truncate text-ink-4" title={g.name}>
                {g.name}
              </span>
            </div>
            <GaugeRow
              label={t('res.gpuUtil')}
              pct={g.util_pct}
              display={g.util_pct != null ? `${g.util_pct}%` : '—'}
            />
            {g.mem_total_mb != null && (
              <GaugeRow
                label={t('res.vram')}
                pct={vramPct}
                display={
                  vramUsed != null ? `${mibToGib(vramUsed)}/${mibToGib(g.mem_total_mb)}G` : '—'
                }
              />
            )}
          </div>
        )
      })}
      {r.ram_total_mb != null && (
        <GaugeRow
          label={t('res.ram')}
          pct={ramPct}
          display={
            r.ram_free_mb != null
              ? `${mibToGib(r.ram_total_mb - r.ram_free_mb)}/${mibToGib(r.ram_total_mb)}G`
              : '—'
          }
        />
      )}
      {r.load1 != null && (
        <div className="flex items-center gap-3">
          <span className="w-16 shrink-0 text-[11px] text-ink-4" title={t('res.loadAvgTitle')}>
            {t('res.loadAvg')}
          </span>
          <span className="num-display text-base text-ink-1">
            {r.load1.toFixed(1)}
            {cores != null && <span className="text-ink-4"> {t('res.cores', { n: cores })}</span>}
          </span>
        </div>
      )}
    </div>
  )
}

function DeclaredChips({ d }: { d: EntryDetail['declared'] }) {
  const { t } = useI18n()
  return (
    <>
      {d.gpu_mem_mb != null && <Pill>{t('decl.gpu', { v: mibToGib(d.gpu_mem_mb) })}</Pill>}
      {d.ram_mb != null && <Pill>{t('decl.ram', { v: mibToGib(d.ram_mb) })}</Pill>}
      {d.cpu_cores != null && <Pill>{t('decl.cpu', { n: d.cpu_cores })}</Pill>}
      {d.retry != null && <Pill>{t('decl.retry', { n: d.retry })}</Pill>}
    </>
  )
}

function UsageChips({ u }: { u: NonNullable<EntryDetail['usage']> }) {
  const { t } = useI18n()
  return (
    <>
      {Object.entries(u.peak_vram_mb).map(([k, v]) => (
        <Pill key={k} tone="zinc" title={t('res.peakVram', { gpu: k })}>
          <span className="font-num">{t('res.vramOf', { v: mibToGib(v) })}</span>
        </Pill>
      ))}
      <Pill tone="zinc" title={t('res.peakRam')}>
        <span className="font-num">{t('res.ramOf', { v: mibToGib(u.peak_ram_mb) })}</span>
      </Pill>
      <Pill tone="zinc">
        <span className="font-num">{t('res.coreTime', { m: Math.floor(u.cpu_seconds / 60) })}</span>
      </Pill>
    </>
  )
}

/** Expandable pending/running entry list with declared+usage chips
 *  and a per-entry open hook: `onLogs` tails a running entry's log,
 *  `onPeek` opens a queued entry's task.sh — the script is the only
 *  artifact a pending entry has. `openName` marks the entry whose
 *  pane is currently shown. */
export function EntryList({
  label,
  entries,
  onLogs,
  onPeek,
  openName,
}: {
  label: string
  entries: EntryDetail[]
  onLogs?: (name: string) => void
  onPeek?: (name: string) => void
  openName?: string | null
}) {
  const { t } = useI18n()
  const onOpen = onLogs ?? onPeek
  const hint = onLogs ? t('common.log') : t('common.script')
  if (entries.length === 0) return null
  return (
    <details className="group">
      <summary className="flex cursor-pointer select-none items-center gap-1.5 text-[11px] text-ink-4 transition-colors hover:text-ink-2">
        <span className="caret text-[9px]">▶</span> {label}
        <span className="font-num text-ink-4">{entries.length}</span>
      </summary>
      <div className="detail-body">
        <div>
          <ul className="mt-1.5 space-y-1.5">
            {entries.map((e) => (
              <li
                key={e.name}
                role={onOpen ? 'button' : undefined}
                tabIndex={onOpen ? 0 : undefined}
                onClick={onOpen ? () => onOpen(e.name) : undefined}
                onKeyDown={
                  onOpen
                    ? (ev) => {
                        if (ev.key === 'Enter' || ev.key === ' ') {
                          ev.preventDefault()
                          onOpen(e.name)
                        }
                      }
                    : undefined
                }
                className={`group/entry rounded-lg px-2.5 py-1.5 ring-1 transition-all duration-150 ${
                  openName === e.name
                    ? 'bg-hover-2 ring-signal-bd'
                    : `bg-hover ring-edge-soft ${onOpen ? 'hover:bg-hover-2 hover:ring-edge' : ''}`
                } ${onOpen ? 'cursor-pointer' : ''}`}
              >
                <div className="flex items-center gap-1">
                  {/* name stays a copy button — stop the click from
                      also opening the log */}
                  <span
                    onClick={(ev) => ev.stopPropagation()}
                    onKeyDown={(ev) => ev.stopPropagation()}
                  >
                    <CopyText text={e.name} className="text-xs text-ink-2" />
                  </span>
                  <span className="flex-1" />
                  {onOpen && (
                    <span
                      className={`text-[10px] transition-colors ${
                        openName === e.name
                          ? 'text-signal-fg'
                          : 'text-ink-4 group-hover/entry:text-signal-fg'
                      }`}
                    >
                      {openName === e.name ? `${hint} ▾` : `${hint} ▸`}
                    </span>
                  )}
                </div>
                <div className="mt-1 flex flex-wrap gap-1">
                  {e.held && (
                    <Pill tone="amber">
                      {e.hold_reason
                        ? t('spool.heldReason', { reason: e.hold_reason })
                        : t('spool.held')}
                    </Pill>
                  )}
                  <DeclaredChips d={e.declared} />
                  {e.usage && <UsageChips u={e.usage} />}
                </div>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </details>
  )
}

export function RecentList({
  label,
  tone,
  items,
  onLogs,
  openName,
}: {
  label: string
  tone: 'emerald' | 'rose'
  items: ExitRecord[]
  onLogs?: (name: string) => void
  openName?: string | null
}) {
  if (items.length === 0) return null
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px]">
      <Muted>{label}</Muted>
      {items.slice(0, 6).map((x) => (
        <span
          key={x.name}
          role={onLogs ? 'button' : undefined}
          tabIndex={onLogs ? 0 : undefined}
          onClick={onLogs ? () => onLogs(x.name) : undefined}
          onKeyDown={
            onLogs
              ? (ev) => {
                  if (ev.key === 'Enter' || ev.key === ' ') {
                    ev.preventDefault()
                    onLogs(x.name)
                  }
                }
              : undefined
          }
          className={`group/x inline-flex items-center gap-1 rounded-md px-1 -mx-1 transition-colors ${
            openName === x.name ? 'bg-hover-2' : onLogs ? 'hover:bg-hover-2' : ''
          } ${onLogs ? 'cursor-pointer' : ''}`}
        >
          <span onClick={(ev) => ev.stopPropagation()} onKeyDown={(ev) => ev.stopPropagation()}>
            <CopyText text={x.name} className="max-w-36 text-ink-3" />
          </span>
          <Pill tone={x.rc === 0 && tone === 'emerald' ? 'emerald' : 'rose'}>
            <span className="font-num">rc={x.rc}</span>
          </Pill>
          {onLogs && (
            <span
              className={`text-[10px] text-ink-4 transition-opacity ${
                openName === x.name ? 'opacity-100' : 'opacity-0 group-hover/x:opacity-100'
              }`}
            >
              {openName === x.name ? '▾' : '▸'}
            </span>
          )}
        </span>
      ))}
      {items.length > 6 && <Muted>+{items.length - 6}</Muted>}
    </div>
  )
}

/** Live-ish log tail: fetches on open and again on every snapshot
 *  tick — a running task's log grows under you. Line-level tinting
 *  marks errors rose and progress lines signal. */
export function LogPane({
  client,
  queue,
  pattern,
  snapAt,
  onClose,
}: {
  client: string
  queue: string | null
  pattern: string | null
  snapAt: number
  onClose: () => void
}) {
  const { t } = useI18n()
  const [tail, setTail] = useState(200)
  const [text, setText] = useState<string | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const box = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  const inFlight = useRef(false)

  useEffect(() => {
    // A snapAt tick landing mid-fetch must skip: each fetch is a
    // remote ssh op, and overlapping polls would stack handshakes
    // on a slow password-auth client instead of taking turns.
    if (inFlight.current) return
    let cancelled = false
    inFlight.current = true
    fetchLogs(client, queue, pattern, tail)
      .then((r) => {
        if (cancelled) return
        if (r.ok) {
          setText(r.text ?? '')
          setErr(null)
        } else {
          setErr(r.error ?? t('probe.unknownError'))
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) setErr(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        inFlight.current = false
      })
    return () => {
      cancelled = true
    }
  }, [client, queue, pattern, tail, snapAt])

  useEffect(() => {
    const el = box.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [text])

  const tint = (line: string): string => {
    if (/\b(error|traceback|failed|panic|exception)\b/i.test(line)) return 'text-[var(--c-rse)]'
    if (/\b(step|puzzle|epoch|iter|batch|solved|progress|gen|fitness|loss|eval|acc)\b/i.test(line))
      return 'text-signal-fg'
    return 'text-ink-3'
  }

  return (
    <div className="mt-3 rounded-xl border border-edge bg-canvas/60 p-3">
      <div className="mb-2 flex items-center gap-2 text-[11px]">
        <Dot tone="signal" pulse />
        <span className="font-num text-ink-2">{pattern ?? t('common.daemon')}</span>
        <Muted>{t('common.logTail')}</Muted>
        <span className="flex-1" />
        {[100, 200, 500, 1000].map((n) => (
          <button
            key={n}
            onClick={() => setTail(n)}
            className={`font-num rounded px-1.5 text-[10px] transition-colors ${tail === n ? 'bg-hover-2 text-ink-1' : 'text-ink-4 hover:text-ink-2'}`}
          >
            {n}
          </button>
        ))}
        <button
          onClick={onClose}
          className="ml-1 rounded px-1.5 text-[11px] text-ink-4 transition-colors hover:bg-hover-2 hover:text-ink-2"
        >
          ✕
        </button>
      </div>
      <div
        ref={box}
        onScroll={(e) => {
          const el = e.currentTarget
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24
        }}
        className="max-h-72 overflow-y-auto pr-1"
      >
        {err ? (
          <Empty>{err}</Empty>
        ) : text === null ? (
          <Empty>{t('state.loading')}</Empty>
        ) : text === '' ? (
          <Empty>{t('state.logEmpty')}</Empty>
        ) : (
          <pre className="font-num whitespace-pre-wrap text-[11px] leading-relaxed">
            {text.split('\n').map((l, i) => (
              <div key={i} className={tint(l)}>
                {l}
              </div>
            ))}
          </pre>
        )}
      </div>
    </div>
  )
}

/** A queued task's task.sh — the pending track's only artifact, so
 *  the script is what an entry click has to show. Fetched once per
 *  open: unlike a running log the file never grows under you. The
 *  `# shunt:`/`# shunt-env:` directive head tints signal — it's the
 *  rendered contract (id, workdir, env, timeout, retry); other
 *  comment lines fade to muted. */
export function ScriptPane({
  client,
  queue,
  name,
  onClose,
}: {
  client: string
  queue: string | null
  name: string | null
  onClose: () => void
}) {
  const { t } = useI18n()
  const [text, setText] = useState<string | null>(null)
  const [err, setErr] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    fetchScript(client, queue, name ?? '')
      .then((r) => {
        if (cancelled) return
        if (r.ok) {
          setText(r.text ?? '')
          setErr(null)
        } else {
          setErr(r.error ?? t('probe.unknownError'))
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) setErr(e instanceof Error ? e.message : String(e))
      })
    return () => {
      cancelled = true
    }
  }, [client, queue, name])

  const tint = (line: string): string => {
    if (/^\s*#\s*shunt(-env)?:/.test(line)) return 'text-signal-fg'
    if (/^\s*#/.test(line)) return 'text-ink-4'
    return 'text-ink-2'
  }

  return (
    <div className="mt-3 rounded-xl border border-edge bg-canvas/60 p-3">
      <div className="mb-2 flex items-center gap-2 text-[11px]">
        <Dot tone="signal" />
        <span className="font-num text-ink-2">{name ?? 'task.sh'}</span>
        <Muted>{t('common.script')}</Muted>
        <span className="flex-1" />
        <button
          onClick={onClose}
          className="ml-1 rounded px-1.5 text-[11px] text-ink-4 transition-colors hover:bg-hover-2 hover:text-ink-2"
        >
          ✕
        </button>
      </div>
      <div className="max-h-72 overflow-y-auto pr-1">
        {err ? (
          <Empty>{err}</Empty>
        ) : text === null ? (
          <Empty>{t('state.loading')}</Empty>
        ) : text === '' ? (
          <Empty>{t('state.scriptEmpty')}</Empty>
        ) : (
          <pre className="font-num whitespace-pre-wrap text-[11px] leading-relaxed">
            {text.split('\n').map((l, i) => (
              <div key={i} className={tint(l)}>
                {l}
              </div>
            ))}
          </pre>
        )}
      </div>
    </div>
  )
}
