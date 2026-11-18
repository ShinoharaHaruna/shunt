import { useEffect, useState } from 'react'
import type { ClientStatus, Declared, StatusReport, TaskUsage } from '../types'
import { fetchScript } from '../api'
import { Card, CopyText, Dot, Empty, Muted, Pill } from './ui'
import { DeclaredChips, LogPane, UsageChips } from './queueDetail'
import { useI18n } from '../i18n'

/** A task's script can outgrow the pane by an order of magnitude —
 *  past this many lines the body shows a head slice and the footer
 *  offers the rest, so one giant heredoc can't bury the log column. */
const SCRIPT_HEAD_LINES = 80

interface Found {
  state: 'pending' | 'running' | 'done' | 'failed'
  declared: Declared
  usage: TaskUsage | null
  rc?: number
  held?: boolean
  holdReason?: string | null
}

/** A task can sit in any of the four tracks; find which one and
 *  normalize EntryDetail vs ExitRecord into one shape. A null here
 *  is not an error — task files persist after the entry rolls off
 *  `done_recent`, and the script/log fetches below still succeed. */
function findEntry(rep: StatusReport | null | undefined, task: string): Found | null {
  if (!rep) return null
  const p = rep.pending_detail.find((e) => e.name === task)
  if (p)
    return {
      state: 'pending',
      declared: p.declared,
      usage: p.usage,
      held: p.held,
      holdReason: p.hold_reason,
    }
  const r = rep.running_detail.find((e) => e.name === task)
  if (r)
    return {
      state: 'running',
      declared: r.declared,
      usage: r.usage,
      held: r.held,
      holdReason: r.hold_reason,
    }
  const d = rep.done_recent.find((e) => e.name === task)
  if (d) return { state: 'done', declared: d.declared, usage: d.usage, rc: d.rc }
  const f = rep.failed_recent.find((e) => e.name === task)
  if (f) return { state: 'failed', declared: f.declared, usage: f.usage, rc: f.rc }
  return null
}

/** The task's task.sh, fetched once — unlike a running log the file
 *  never grows under you. The `# shunt:`/`# shunt-env:` directive
 *  head tints signal — it's the rendered contract (id, workdir, env,
 *  timeout, retry); other comment lines fade to muted. */
function ScriptBlock({ client, queue, name }: { client: string; queue: string; name: string }) {
  const { t } = useI18n()
  const [text, setText] = useState<string | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [all, setAll] = useState(false)

  useEffect(() => {
    let cancelled = false
    fetchScript(client, queue, name)
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

  const lines = text?.split('\n') ?? []
  const shown = all ? lines : lines.slice(0, SCRIPT_HEAD_LINES)

  return (
    <div className="rounded-xl border border-edge bg-canvas/60 p-3">
      <div className="mb-2 flex items-center gap-2 text-[11px]">
        <Dot tone="signal" />
        <span className="font-num text-ink-2">task.sh</span>
        <Muted>{t('common.script')}</Muted>
      </div>
      <div className="max-h-[36rem] overflow-y-auto pr-1">
        {err ? (
          <Empty>{err}</Empty>
        ) : text === null ? (
          <Empty>{t('state.loading')}</Empty>
        ) : text === '' ? (
          <Empty>{t('state.scriptEmpty')}</Empty>
        ) : (
          <pre className="font-num whitespace-pre-wrap text-[11px] leading-relaxed">
            {shown.map((l, i) => (
              <div key={i} className={tint(l)}>
                {l}
              </div>
            ))}
          </pre>
        )}
      </div>
      {lines.length > SCRIPT_HEAD_LINES && (
        <button
          onClick={() => setAll((v) => !v)}
          className="font-num mt-2 rounded px-1.5 text-[10px] text-ink-4 transition-colors hover:text-ink-2"
        >
          {all ? t('task.collapse') : t('task.showAll', { n: lines.length })}
        </button>
      )}
    </div>
  )
}

export function TaskPage({
  clients,
  snapAt,
  client,
  queue,
  task,
}: {
  clients: Record<string, ClientStatus> | null
  snapAt: number
  client: string
  queue: string
  task: string
}) {
  const { t } = useI18n()
  if (clients === null) {
    return (
      <Card>
        <Empty>{t('state.waitingSweep')}</Empty>
      </Card>
    )
  }
  const found = findEntry(clients[client]?.queues?.[queue]?.report, task)
  const enc = encodeURIComponent
  return (
    <div>
      <a
        href={`#/queues/${enc(client)}/${enc(queue)}`}
        className="text-[11px] text-ink-4 transition-colors hover:text-ink-2"
      >
        ‹ {client}/{queue}
      </a>
      <div className="mt-2 flex min-w-0 flex-wrap items-center gap-2">
        <h2 className="text-lg font-semibold tracking-tight text-ink-1">
          <CopyText text={task} />
        </h2>
        {found?.state === 'pending' && <Pill tone="amber">{t('queue.pending')}</Pill>}
        {found?.state === 'running' && <Pill tone="signal">{t('queue.running')}</Pill>}
        {(found?.state === 'done' || found?.state === 'failed') && (
          <>
            <Pill>{found.state === 'done' ? t('queue.done') : t('queue.failed')}</Pill>
            <Pill tone={found.rc === 0 && found.state === 'done' ? 'emerald' : 'rose'}>
              <span className="font-num">rc={found.rc}</span>
            </Pill>
          </>
        )}
        {found?.held && (
          <Pill tone="amber">
            {found.holdReason
              ? t('spool.heldReason', { reason: found.holdReason })
              : t('spool.held')}
          </Pill>
        )}
      </div>
      {found === null && <Muted className="mt-1 block text-[11px]">{t('task.notInSnap')}</Muted>}
      <div className="mt-5">
        <Muted className="mb-1.5 block text-[11px] uppercase tracking-widest">
          {t('task.declared')}
        </Muted>
        <div className="flex flex-wrap gap-1.5">
          {found ? <DeclaredChips d={found.declared} /> : <Muted>—</Muted>}
        </div>
      </div>
      <div className="mt-4">
        <Muted className="mb-1.5 block text-[11px] uppercase tracking-widest">
          {t('task.usage')}
        </Muted>
        <div className="flex flex-wrap gap-1.5">
          {found?.usage ? (
            <>
              <UsageChips u={found.usage} />
              <Pill>
                <span className="font-num">{t('task.samples', { n: found.usage.samples })}</span>
              </Pill>
            </>
          ) : (
            <Muted>—</Muted>
          )}
        </div>
      </div>
      <div className="mt-6 grid gap-4 lg:grid-cols-2">
        <ScriptBlock client={client} queue={queue} name={task} />
        {found?.state === 'pending' ? (
          <div className="flex items-center justify-center rounded-xl border border-edge bg-canvas/60 p-3">
            <Muted>{t('task.noLog')}</Muted>
          </div>
        ) : (
          <LogPane
            client={client}
            queue={queue}
            pattern={task}
            snapAt={snapAt}
            bodyClass="max-h-[36rem]"
          />
        )}
      </div>
    </div>
  )
}
