import { useState } from 'react'
import type { ClientStatus, QueueStatus, StatusReport } from '../types'
import { postExec } from '../api'
import { Card, CopyText, Dot, Empty, Muted, Pill, SectionHeading } from './ui'
import { EntryList, LogPane, RecentList, ResourceBlock } from './queueDetail'
import { useI18n } from '../i18n'

interface Sel {
  client: string
  queue: string
}

function allQueues(
  clients: Record<string, ClientStatus>,
): { client: string; queue: string; q: QueueStatus }[] {
  const out: { client: string; queue: string; q: QueueStatus }[] = []
  for (const [c, row] of Object.entries(clients)) {
    for (const [queue, q] of Object.entries(row.queues ?? {})) out.push({ client: c, queue, q })
  }
  return out
}

function QueueRow({
  client,
  queue,
  q,
  active,
}: {
  client: string
  queue: string
  q: QueueStatus
  active: boolean
}) {
  const { t } = useI18n()
  const rep = q.report
  const pend = rep?.pending_detail?.length ?? rep?.pending?.length ?? 0
  const run = rep?.running_detail?.length ?? rep?.running?.length ?? 0
  const up = rep?.daemon?.alive === true
  return (
    <a
      href={`#/queues/${encodeURIComponent(client)}/${encodeURIComponent(queue)}`}
      title={queue}
      className={`relative block w-full rounded-xl border px-3 py-2 pl-4 text-left transition-all duration-200 ${
        active
          ? 'border-edge bg-hover-2 before:absolute before:left-0 before:top-2 before:bottom-2 before:w-[2px] before:rounded-full before:bg-signal'
          : 'border-transparent hover:translate-x-0.5 hover:bg-hover-2'
      }`}
    >
      <div className="flex items-center gap-2">
        <Dot tone={up ? 'signal' : q.reachable === false ? 'rose' : 'zinc'} />
        <span className="font-num truncate text-[13px] text-ink-2">{queue}</span>
      </div>
      {/* the status dot already says daemon-up; line 2 carries only
          what the dot can't — failure text or live count badges */}
      {(() => {
        const status = !rep ? (q.error ?? '…') : rep.daemon.alive ? null : t('common.daemonDown')
        if (status === null && run === 0 && pend === 0) return null
        return (
          <div className="mt-1 flex items-center gap-1">
            {status !== null && <Muted className="truncate text-[11px]">{status}</Muted>}
            {run > 0 && (
              <Pill tone="signal">
                <span className="font-num">{t('common.running', { n: run })}</span>
              </Pill>
            )}
            {pend > 0 && (
              <Pill tone="amber">
                <span className="font-num">{t('common.pending', { n: pend })}</span>
              </Pill>
            )}
          </div>
        )
      })()}
    </a>
  )
}

function CompatPill({ compat }: { compat: string | null | undefined }) {
  const { t } = useI18n()
  if (!compat) return null
  return (
    <Pill tone={compat === 'ok' ? 'emerald' : 'amber'}>
      {t('common.compat', { v: compat.replace(/^[^\w]*/, '') })}
    </Pill>
  )
}

function StatusChips({ rep, compat }: { rep: StatusReport; compat?: string | null }) {
  const { t } = useI18n()
  return (
    <div className="mt-1 flex flex-wrap items-center gap-1.5 text-[11px]">
      <Pill tone={rep.daemon.alive ? 'emerald' : 'zinc'}>
        {rep.daemon.alive
          ? t('common.daemonPid', { pid: rep.daemon.pid ?? '?' })
          : t('common.daemonDown')}
      </Pill>
      <Pill>
        <span className="font-num">{t('common.workers', { n: rep.workers.value })}</span>
      </Pill>
      {rep.needs_glibc && (
        <Pill tone="amber">{t('common.needsGlibc', { v: rep.needs_glibc })}</Pill>
      )}
      <CompatPill compat={compat} />
      {rep.stop && <Pill tone="amber">{t('common.stop')}</Pill>}
      {rep.shutdown.armed && (
        <Pill tone="rose">
          <span className="font-num">{t('common.shutdownGrace', { s: rep.shutdown.grace_s })}</span>
        </Pill>
      )}
    </div>
  )
}

const PROBES: [string, string][] = [
  ['ps', 'ps -eo pid,pgid,etime,%cpu,stat,cmd --sort=-%cpu | head -25'],
  ['nvidia-smi', 'nvidia-smi'],
  ['df', 'df -h /'],
  ['free', 'free -h'],
  ['uptime', 'uptime'],
]

/** `client exec` passthrough — read-only probes, one shell string at
 *  a time. Output is verbatim; the transport timeout caps runaway
 *  commands the same way the CLI does. */
function ProbeSection({ client }: { client: string }) {
  const { t } = useI18n()
  const [cmd, setCmd] = useState('')
  const [out, setOut] = useState<string | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const run = (c: string) => {
    if (!c.trim() || busy) return
    setBusy(true)
    setErr(null)
    postExec(client, c)
      .then((r) => {
        if (r.ok) setOut(r.out ?? '')
        else setErr(r.error ?? t('probe.unknownError'))
      })
      .catch((e: unknown) => setErr(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false))
  }

  return (
    <div className="mt-4 border-t border-edge-soft pt-3">
      <div className="mb-2 flex items-center gap-2 text-[11px]">
        <Muted>{t('probe.label', { client })}</Muted>
        {PROBES.map(([label, c]) => (
          <button
            key={label}
            onClick={() => {
              setCmd(c)
              run(c)
            }}
            className="font-num rounded px-1.5 py-0.5 text-[10px] text-ink-4 ring-1 ring-edge transition-all duration-150 hover:-translate-y-px hover:bg-hover hover:text-ink-2 active:translate-y-0"
          >
            {label}
          </button>
        ))}
      </div>
      <form
        className="flex gap-1.5"
        onSubmit={(e) => {
          e.preventDefault()
          run(cmd)
        }}
      >
        <input
          value={cmd}
          onChange={(e) => setCmd(e.target.value)}
          placeholder={t('probe.placeholder', { client })}
          spellCheck={false}
          className="font-num min-w-0 flex-1 rounded-lg border border-edge bg-hover px-2.5 py-1 text-[12px] text-ink-2 placeholder:text-ink-4 focus:border-edge focus:outline-none"
        />
        <button
          type="submit"
          disabled={busy || !cmd.trim()}
          className="rounded-lg border border-edge px-2.5 text-[11px] text-ink-2 transition-all duration-150 hover:bg-hover active:scale-[0.97] disabled:opacity-40"
        >
          {busy ? '…' : t('probe.run')}
        </button>
      </form>
      {(err !== null || out !== null) && (
        <pre className="font-num mt-2 max-h-64 overflow-y-auto whitespace-pre-wrap rounded-lg bg-hover p-2.5 text-[11px] leading-relaxed ring-1 ring-edge-soft">
          {err !== null ? (
            <span className="text-[var(--c-rse)]">{err}</span>
          ) : out === '' ? (
            t('state.noOutput')
          ) : (
            out
          )}
        </pre>
      )}
    </div>
  )
}

function QueueDetail({
  client,
  queue,
  q,
  snapAt,
  facts,
}: {
  client: string
  queue: string
  q: QueueStatus
  snapAt: number
  facts?: Record<string, unknown>
}) {
  // Which section's log is open, and for which entry. Clicking the
  // same entry again toggles it closed; the pane renders inline
  // right under its owning section, not at the foot of the card.
  const [log, setLog] = useState<{
    kind: 'running' | 'done' | 'failed' | 'daemon'
    pattern: string | null
  } | null>(null)
  const rep = q.report
  const toggle = (kind: 'running' | 'done' | 'failed' | 'daemon', pattern: string | null) =>
    setLog((prev) =>
      prev && prev.kind === kind && prev.pattern === pattern ? null : { kind, pattern },
    )
  const pane = (kind: 'running' | 'done' | 'failed' | 'daemon') =>
    log?.kind === kind ? (
      <div key={`${kind}:${log.pattern ?? ''}`} className="rise">
        <LogPane
          client={client}
          queue={queue}
          pattern={log.pattern}
          snapAt={snapAt}
          onClose={() => setLog(null)}
        />
      </div>
    ) : null
  const openName = (kind: 'running' | 'done' | 'failed') =>
    log?.kind === kind ? log.pattern : null
  const { t } = useI18n()
  return (
    <div>
      <div className="flex min-w-0 items-baseline gap-2.5">
        <h3 className="shrink-0 text-lg font-semibold tracking-tight text-ink-1">
          <CopyText text={`${client}/${queue}`} />
        </h3>
        <Muted className="font-num min-w-0 truncate text-[11px]">{q.qroot}</Muted>
      </div>
      {rep ? (
        <>
          <StatusChips rep={rep} compat={q.compat} />
          <ResourceBlock
            report={rep}
            cores={
              typeof facts?.cores === 'number' ? facts.cores : Number(facts?.cores) || undefined
            }
          />
          <div className="mt-4 space-y-3">
            <EntryList
              label={t('queue.running')}
              entries={rep.running_detail}
              onLogs={(n) => toggle('running', n)}
              openName={openName('running')}
            />
            {pane('running')}
            <EntryList label={t('queue.pending')} entries={rep.pending_detail} />
            <RecentList
              label={t('queue.done')}
              tone="emerald"
              items={rep.done_recent}
              onLogs={(n) => toggle('done', n)}
              openName={openName('done')}
            />
            {pane('done')}
            <RecentList
              label={t('queue.failed')}
              tone="rose"
              items={rep.failed_recent}
              onLogs={(n) => toggle('failed', n)}
              openName={openName('failed')}
            />
            {pane('failed')}
          </div>
        </>
      ) : (
        <Empty>
          {q.error ?? (q.reachable ? t('queue.waitingFirstProbe') : t('state.unreachable'))}
        </Empty>
      )}
      <div className="mt-3">
        {log?.kind === 'daemon'
          ? pane('daemon')
          : rep && (
              <button
                onClick={() => toggle('daemon', null)}
                className="font-num rounded px-1.5 text-[10px] text-ink-4 transition-colors hover:bg-hover-2 hover:text-ink-2"
              >
                {t('common.daemonLog')}
              </button>
            )}
      </div>
      <ProbeSection client={client} />
    </div>
  )
}

export function QueuesPage({
  clients,
  snapAt,
  sel,
}: {
  clients: Record<string, ClientStatus> | null
  snapAt: number
  sel: Sel | null
}) {
  const { t } = useI18n()
  // Reset nothing here — QueueDetail remounts per selection via key.
  if (clients === null) {
    return (
      <div>
        <SectionHeading title={t('nav.queues')} />
        <Card>
          <Empty>{t('state.waitingSweep')}</Empty>
        </Card>
      </div>
    )
  }
  const all = allQueues(clients)
  const selQ = sel ? clients[sel.client]?.queues?.[sel.queue] : undefined
  const byClient = new Map<string, { queue: string; q: QueueStatus }[]>()
  for (const e of all) {
    const arr = byClient.get(e.client) ?? []
    arr.push({ queue: e.queue, q: e.q })
    byClient.set(e.client, arr)
  }
  return (
    <div>
      <SectionHeading title={t('nav.queues')} count={all.length || undefined} />
      <div className="grid gap-6 md:grid-cols-[13rem_1fr]">
        <div className="min-w-0 space-y-4">
          {[...byClient.entries()].map(([client, qs]) => (
            <div key={client}>
              <Muted className="mb-1 block px-3 text-[11px] uppercase tracking-widest">
                {client}
              </Muted>
              <div className="space-y-1">
                {qs.map(({ queue, q }) => (
                  <QueueRow
                    key={queue}
                    client={client}
                    queue={queue}
                    q={q}
                    active={sel?.client === client && sel?.queue === queue}
                  />
                ))}
              </div>
            </div>
          ))}
        </div>
        <Card className="min-w-0">
          {sel && selQ ? (
            <QueueDetail
              key={`${sel.client}/${sel.queue}`}
              client={sel.client}
              queue={sel.queue}
              q={selQ}
              snapAt={snapAt}
              facts={clients[sel.client]?.facts ?? undefined}
            />
          ) : sel && !selQ ? (
            <Empty>{t('queue.notFound', { name: `${sel.client}/${sel.queue}` })}</Empty>
          ) : (
            <Empty>{t('queue.selectQueue')}</Empty>
          )}
        </Card>
      </div>
    </div>
  )
}
