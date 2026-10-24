import type { ClientStatus } from '../types'
import { humanizeSecs } from '../fmt'
import { Card, CopyText, Dot, Muted, Pill } from './ui'
import { useI18n } from '../i18n'

/** Fleet-level client card — reachability, facts, expiry, billing,
 *  and queue chips that deep-link into '#/queues/<client>/<queue>'.
 *  Per-queue detail (resources, entries, logs) lives on that page. */
export function ClientCard({ name, client }: { name: string; client: ClientStatus }) {
  const { t } = useI18n()
  const up = client.reachable === true
  const down = client.reachable === false
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
      {down && <p className="mt-2 text-xs text-ink-4">{client.error ?? t('state.unreachable')}</p>}
    </Card>
  )
}
