import { useState } from 'react'
import type { ClientStatus, ProbeSkip } from '../types'
import { humanizeSecs } from '../fmt'
import { reprobe } from '../api'
import { Card, CopyText, Dot, Muted, Pill } from './ui'
import { useI18n } from '../i18n'

type T = ReturnType<typeof useI18n>['t']

/** Label for a row the background sweep deliberately skipped: why it
 *  is not being asked, in the reader's language. */
function skipLabel(skip: ProbeSkip, t: T): string {
  if (skip.reason === 'disabled') return t('skip.disabled')
  if (skip.reason === 'expired') return t('skip.expired')
  if (skip.reason === 'quarantined' || skip.reason === 'gave_up')
    return t('skip.quarantined', { n: skip.fails ?? 0 })
  return t('skip.backoff', { t: humanizeSecs(skip.retry_in_s ?? 0) })
}

/** Expired / given-up / backed-off row: why the sweep is not asking,
 *  plus the one deliberate action that spends an ssh anyway
 *  (`POST /api/reprobe`). The state lives on the card, not on this
 *  block: the forced answer lands in the snapshot for one tick (so
 *  `skip` briefly disappears) and the outcome the click produced must
 *  survive that flip. */
function ProbeLine({
  skip,
  probe,
  onProbe,
}: {
  skip?: ProbeSkip | null
  probe: { busy: boolean; error?: string; result?: string }
  onProbe: () => void
}) {
  const { t } = useI18n()
  return (
    <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-ink-4">
      {/* No label while a forced answer sits in the snapshot: the row
          above already carries that answer, and repeating it as
          "unreachable" adds noise until the next sweep re-skips. */}
      {skip && <span>{skipLabel(skip, t)}</span>}
      {skip?.reason !== 'disabled' && (
        <button
          disabled={probe.busy}
          onClick={onProbe}
          className="rounded-full border border-edge px-2 py-0.5 text-[11px] font-medium text-ink-2 transition-colors hover:bg-hover-2 hover:text-ink-1 disabled:opacity-50"
        >
          {probe.busy ? t('state.loading') : t('probe.recheck')}
        </button>
      )}
      {(probe.result || probe.error) && (
        <span className={`w-full ${probe.error ? 'text-c-rse' : 'text-ink-3'}`}>
          {probe.error ?? probe.result}
        </span>
      )}
    </div>
  )
}

/** Fleet-level client card — reachability, facts, expiry, billing,
 *  and queue chips that deep-link into '#/queues/<client>/<queue>'.
 *  Per-queue detail (resources, entries, logs) lives on that page. */
export function ClientCard({ name, client }: { name: string; client: ClientStatus }) {
  const { t } = useI18n()
  const [probe, setProbe] = useState<{ busy: boolean; error?: string; result?: string }>({
    busy: false,
  })
  const up = client.reachable === true
  const down = client.reachable === false
  // The row is dark whenever the sweep stopped asking (skip) or the user
  // already forced an answer; a plain unreachable row keeps retrying by
  // itself and needs no button.
  const probeLine =
    down && (client.skip != null || probe.busy || probe.result != null || probe.error != null)
  const runProbe = async () => {
    setProbe({ busy: true })
    const r = await reprobe(name)
    const st = r.status
    setProbe({
      busy: false,
      error: r.ok ? undefined : (r.error ?? t('probe.unknownError')),
      result: st
        ? st.reachable
          ? t('probe.ok')
          : (st.error ?? t('state.unreachable')).split('\n')[0].slice(0, 160)
        : undefined,
    })
  }
  const facts = client.facts
    ? Object.entries(client.facts)
        .map(([k, v]) => `${k} ${String(v)}`)
        .join(' · ')
    : ''
  const connFail = client.connect_failures ?? 0
  const queues = Object.entries(client.queues ?? {})
  return (
    <Card className={down ? 'opacity-70 saturate-50' : ''}>
      <div className="mb-1.5 flex flex-wrap items-center gap-x-2 gap-y-1">
        <Dot tone={up ? 'signal' : down ? 'rose' : 'zinc'} />
        <h3 className="shrink-0 text-[15px] font-semibold tracking-tight text-ink-1">
          <CopyText text={name} />
        </h3>
        <span className="flex-1" />
        {client.billing && (
          <Pill>
            <span className="font-num">
              {client.billing.hourly} {client.billing.currency}/h
            </span>
          </Pill>
        )}
        {client.expires_in_s != null && (
          <Pill>
            <span className="font-num">
              {client.expires_in_s < 0 ? t('state.expired') : humanizeSecs(client.expires_in_s)}
            </span>
          </Pill>
        )}
        {client.endpoint_rotates && <Pill>{t('common.rotating')}</Pill>}
        {connFail > 0 && (
          <Pill tone="rose">
            <span className="font-num">{t('common.connFail', { n: connFail })}</span>
          </Pill>
        )}
      </div>
      {facts && (
        <p className="font-num truncate text-[11px] text-ink-4" title={facts}>
          {facts}
        </p>
      )}
      {/* A skipped row has no queue answer to show — the skip line below
          says why, and "no queues" would read as the box reporting none. */}
      {(queues.length > 0 || !client.skip) && (
        <div className="mt-3 space-y-1 border-t border-edge-soft pt-2.5">
          {queues.length === 0 && <Muted className="text-[11px]">{t('machines.noQueues')}</Muted>}
          {queues.map(([qn, q]) => {
            const rep = q.report
            const pend = rep?.pending_detail?.length ?? rep?.pending?.length ?? 0
            const run = rep?.running_detail?.length ?? rep?.running?.length ?? 0
            const daemonUp = rep?.daemon?.alive === true
            return (
              <a
                key={qn}
                href={`#/queues/${encodeURIComponent(name)}/${encodeURIComponent(qn)}`}
                className="group flex items-center gap-2.5 rounded-lg px-2 py-1.5 text-[12px] transition-colors hover:bg-hover"
              >
                <Dot tone={daemonUp ? 'signal' : q.reachable ? 'zinc' : 'rose'} />
                <span className="font-num w-20 shrink-0 uppercase tracking-[0.08em] text-ink-2">
                  {qn}
                </span>
                <Muted className="truncate text-[11px]">
                  {rep
                    ? rep.daemon.alive
                      ? t('common.daemonPid', { pid: rep.daemon.pid ?? '?' })
                      : t('common.daemonDown')
                    : (q.error ?? '…')}
                </Muted>
                <span className="flex-1" />
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
                <span className="text-[10px] text-ink-4 opacity-0 transition-opacity group-hover:opacity-100">
                  →
                </span>
              </a>
            )
          })}
        </div>
      )}
      {probeLine && <ProbeLine skip={client.skip} probe={probe} onProbe={runProbe} />}
      {down && !probeLine && (
        <p className="mt-2 text-xs text-ink-4">{client.error ?? t('state.unreachable')}</p>
      )}
    </Card>
  )
}
