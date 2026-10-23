import { useState } from 'react'
import type { ClientStatus, QueueStatus, StatusReport } from '../types'
import { postExec } from '../api'
import { Card, CopyText, Dot, Empty, Muted, Pill, SectionHeading } from './ui'
import { EntryList, LogPane, RecentList, ResourceBlock } from './queueDetail'

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
  const rep = q.report
  const pend = rep?.pending_detail?.length ?? rep?.pending?.length ?? 0
  const run = rep?.running_detail?.length ?? rep?.running?.length ?? 0
  const up = rep?.daemon?.alive === true
  return (
    <a
      href={`#/queues/${encodeURIComponent(client)}/${encodeURIComponent(queue)}`}
      className={`relative block w-full rounded-xl border px-3 py-2 pl-4 text-left transition-all duration-200 ${
        active
          ? 'border-edge bg-hover-2 before:absolute before:left-0 before:top-2 before:bottom-2 before:w-[2px] before:rounded-full before:bg-signal'
          : 'border-transparent hover:translate-x-0.5 hover:bg-hover-2'
      }`}
    >
      <div className="flex items-center gap-2">
        <Dot tone={up ? 'signal' : q.reachable === false ? 'rose' : 'zinc'} />
        <span className="font-num truncate text-[13px] text-ink-2">{queue}</span>
        <span className="flex-1" />
        <span className={`font-num text-[11px] ${run > 0 ? 'text-signal-fg' : 'text-ink-4'}`}>
          r{run}
        </span>
        <span className={`font-num text-[11px] ${pend > 0 ? 'text-[var(--c-amb)]' : 'text-ink-4'}`}>
          p{pend}
        </span>
      </div>
      <Muted className="mt-0.5 block truncate text-[11px]">
        {rep
          ? rep.daemon.alive
            ? `daemon · ${rep.workers.value}w`
            : 'daemon down'
          : (q.error ?? '…')}
      </Muted>
    </a>
  )
}

function CompatPill({ compat }: { compat: string | null | undefined }) {
  if (!compat) return null
  return (
    <Pill tone={compat === 'ok' ? 'emerald' : 'amber'}>compat {compat.replace(/^[^\w]*/, '')}</Pill>
  )
}

function StatusChips({ rep, compat }: { rep: StatusReport; compat?: string | null }) {
  return (
    <div className="mt-1 flex flex-wrap items-center gap-1.5 text-[11px]">
      <Pill tone={rep.daemon.alive ? 'emerald' : 'zinc'}>
        {rep.daemon.alive ? `daemon · pid ${rep.daemon.pid ?? '?'}` : 'daemon down'}
      </Pill>
      <Pill>
        <span className="font-num">w={rep.workers.value}</span>
      </Pill>
      {rep.needs_glibc && <Pill tone="amber">needs glibc {rep.needs_glibc}</Pill>}
      <CompatPill compat={compat} />
      {rep.stop && <Pill tone="amber">stop</Pill>}
      {rep.shutdown.armed && (
        <Pill tone="rose">
          <span className="font-num">shutdown {rep.shutdown.grace_s}s</span>
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
        else setErr(r.error ?? 'unknown error')
      })
      .catch((e: unknown) => setErr(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false))
  }

  return (
    <div className="mt-4 border-t border-edge-soft pt-3">
      <div className="mb-2 flex items-center gap-2 text-[11px]">
        <Muted>probe · {client}</Muted>
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
          placeholder={`sh -c "…" on ${client}`}
          spellCheck={false}
          className="font-num min-w-0 flex-1 rounded-lg border border-edge bg-hover px-2.5 py-1 text-[12px] text-ink-2 placeholder:text-ink-4 focus:border-edge focus:outline-none"
        />
        <button
          type="submit"
          disabled={busy || !cmd.trim()}
          className="rounded-lg border border-edge px-2.5 text-[11px] text-ink-2 transition-all duration-150 hover:bg-hover active:scale-[0.97] disabled:opacity-40"
        >
          {busy ? '…' : 'run'}
        </button>
      </form>
      {(err !== null || out !== null) && (
        <pre className="font-num mt-2 max-h-64 overflow-y-auto whitespace-pre-wrap rounded-lg bg-hover p-2.5 text-[11px] leading-relaxed ring-1 ring-edge-soft">
          {err !== null ? (
            <span className="text-[var(--c-rse)]">{err}</span>
          ) : out === '' ? (
            '(no output)'
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
  const [logPattern, setLogPattern] = useState<string | null | 'daemon'>(null)
  const rep = q.report
  const pattern = logPattern === null ? null : logPattern === 'daemon' ? null : logPattern
  const logOpen = logPattern !== null
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
              label="running"
              entries={rep.running_detail}
              onLogs={(n) => setLogPattern(n)}
            />
            <EntryList label="pending" entries={rep.pending_detail} />
            <RecentList
              label="done"
              tone="emerald"
              items={rep.done_recent}
              onLogs={(n) => setLogPattern(n)}
            />
            <RecentList
              label="failed"
              tone="rose"
              items={rep.failed_recent}
              onLogs={(n) => setLogPattern(n)}
            />
          </div>
        </>
      ) : (
        <Empty>{q.error ?? (q.reachable ? 'waiting for first probe' : 'unreachable')}</Empty>
      )}
      <div className="mt-3">
        {logOpen ? (
          <div key={logPattern} className="rise">
            <LogPane
              client={client}
              queue={queue}
              pattern={pattern}
              snapAt={snapAt}
              onClose={() => setLogPattern(null)}
            />
          </div>
        ) : (
          rep && (
            <button
              onClick={() => setLogPattern('daemon')}
              className="font-num rounded px-1.5 text-[10px] text-ink-4 transition-colors hover:bg-hover-2 hover:text-ink-2"
            >
              daemon log
            </button>
          )
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
  // Reset nothing here — QueueDetail remounts per selection via key.
  if (clients === null) {
    return (
      <div>
        <SectionHeading title="Queues" />
        <Card>
          <Empty>waiting for first sweep</Empty>
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
      <SectionHeading title="Queues" count={all.length || undefined} />
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
            <Empty>
              queue <span className="font-num">{`${sel.client}/${sel.queue}`}</span> not found
            </Empty>
          ) : (
            <Empty>select a queue</Empty>
          )}
        </Card>
      </div>
    </div>
  )
}
