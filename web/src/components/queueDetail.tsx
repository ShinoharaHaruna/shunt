import { useEffect, useRef, useState } from 'react'
import type { EntryDetail, ExitRecord, StatusReport } from '../types'
import { coreTime, mibToGib } from '../fmt'
import { fetchLogs } from '../api'
import { Bar, CopyText, Dot, Empty, Muted, Pill } from './ui'
import type { Tone } from './ui'

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
              label="GPU Util"
              pct={g.util_pct}
              display={g.util_pct != null ? `${g.util_pct}%` : '—'}
            />
            {g.mem_total_mb != null && (
              <GaugeRow
                label="vRAM"
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
          label="RAM"
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
          <span
            className="w-16 shrink-0 text-[11px] text-ink-4"
            title="1-min system load average — runnable + uninterruptible tasks competing for CPU; above core count means queueing"
          >
            Load avg
          </span>
          <span className="num-display text-base text-ink-1">
            {r.load1.toFixed(1)}
            {cores != null && <span className="text-ink-4"> / {cores} cores</span>}
          </span>
        </div>
      )}
    </div>
  )
}

function DeclaredChips({ d }: { d: EntryDetail['declared'] }) {
  return (
    <>
      {d.gpu_mem_mb != null && <Pill>gpu {mibToGib(d.gpu_mem_mb)}G</Pill>}
      {d.ram_mb != null && <Pill>ram {mibToGib(d.ram_mb)}G</Pill>}
      {d.cpu_cores != null && <Pill>cpu {d.cpu_cores}</Pill>}
      {d.retry != null && <Pill>retry {d.retry}</Pill>}
    </>
  )
}

function UsageChips({ u }: { u: NonNullable<EntryDetail['usage']> }) {
  return (
    <>
      {Object.entries(u.peak_vram_mb).map(([k, v]) => (
        <Pill key={k} tone="zinc" title={`peak vram ${k}`}>
          <span className="font-num">vram {mibToGib(v)}G</span>
        </Pill>
      ))}
      <Pill tone="zinc" title="peak ram">
        <span className="font-num">ram {mibToGib(u.peak_ram_mb)}G</span>
      </Pill>
      <Pill tone="zinc">
        <span className="font-num">{coreTime(u.cpu_seconds)}</span>
      </Pill>
    </>
  )
}

/** Expandable pending/running entry list with declared+usage chips
 *  and a per-entry logs hook (running entries have live logs to tail). */
export function EntryList({
  label,
  entries,
  onLogs,
}: {
  label: string
  entries: EntryDetail[]
  onLogs?: (name: string) => void
}) {
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
                role={onLogs ? 'button' : undefined}
                tabIndex={onLogs ? 0 : undefined}
                onClick={onLogs ? () => onLogs(e.name) : undefined}
                onKeyDown={
                  onLogs
                    ? (ev) => {
                        if (ev.key === 'Enter' || ev.key === ' ') {
                          ev.preventDefault()
                          onLogs(e.name)
                        }
                      }
                    : undefined
                }
                className={`group/entry rounded-lg bg-hover px-2.5 py-1.5 ring-1 ring-edge-soft transition-all duration-150 ${
                  onLogs ? 'cursor-pointer hover:bg-hover-2 hover:ring-edge' : ''
                }`}
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
                  {onLogs && (
                    <span className="text-[10px] text-ink-4 transition-colors group-hover/entry:text-signal-fg">
                      log ▸
                    </span>
                  )}
                </div>
                <div className="mt-1 flex flex-wrap gap-1">
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
}: {
  label: string
  tone: 'emerald' | 'rose'
  items: ExitRecord[]
  onLogs?: (name: string) => void
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
            onLogs ? 'cursor-pointer hover:bg-hover-2' : ''
          }`}
        >
          <span onClick={(ev) => ev.stopPropagation()} onKeyDown={(ev) => ev.stopPropagation()}>
            <CopyText text={x.name} className="max-w-36 text-ink-3" />
          </span>
          <Pill tone={x.rc === 0 && tone === 'emerald' ? 'emerald' : 'rose'}>
            <span className="font-num">rc={x.rc}</span>
          </Pill>
          {onLogs && (
            <span className="text-[10px] text-ink-4 opacity-0 transition-opacity group-hover/x:opacity-100">
              ▸
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
  const [tail, setTail] = useState(200)
  const [text, setText] = useState<string | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const box = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  useEffect(() => {
    let cancelled = false
    fetchLogs(client, queue, pattern, tail)
      .then((r) => {
        if (cancelled) return
        if (r.ok) {
          setText(r.text ?? '')
          setErr(null)
        } else {
          setErr(r.error ?? 'unknown error')
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) setErr(e instanceof Error ? e.message : String(e))
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
        <span className="font-num text-ink-2">{pattern ?? 'daemon'}</span>
        <Muted>log tail</Muted>
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
          <Empty>loading…</Empty>
        ) : text === '' ? (
          <Empty>log empty</Empty>
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
