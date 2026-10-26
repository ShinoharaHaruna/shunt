import type { SpoolEntry } from '../types'
import { shortTs } from '../fmt'
import { Card, CopyText, Empty, SectionHeading } from './ui'
import { useI18n } from '../i18n'

const th = 'pb-2.5 pr-5 text-left text-[11px] font-medium uppercase tracking-[0.08em] text-ink-4'
const td = 'py-2 pr-5 text-sm text-ink-2'

export function SpoolPanel({ spool }: { spool: { entries?: SpoolEntry[]; error?: string } }) {
  const { t } = useI18n()
  const entries = spool.entries
  return (
    <div>
      <SectionHeading title={t('spool.title')} count={entries?.length} />
      <Card className="p-0">
        {spool.error ? (
          <Empty>{t('spool.error', { err: spool.error })}</Empty>
        ) : !entries ? (
          <Empty>{t('state.waitingSweep')}</Empty>
        ) : entries.length === 0 ? (
          <Empty>{t('spool.empty')}</Empty>
        ) : (
          <div className="overflow-x-auto px-5 py-3">
            <table className="w-full border-collapse">
              <thead>
                <tr className="border-b border-edge-soft">
                  <th className={th}>{t('spool.col.id')}</th>
                  <th className={th}>{t('spool.col.project')}</th>
                  <th className={th}>{t('spool.col.route')}</th>
                  <th className={th}>{t('spool.col.slug')}</th>
                  <th className={th}>{t('spool.col.submitted')}</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => (
                  <tr
                    key={e.id}
                    className="border-b border-edge-soft transition-colors last:border-0 hover:bg-hover"
                  >
                    <td className={`${td} text-xs`}>
                      <CopyText text={e.id} />
                    </td>
                    <td className={td}>{e.project}</td>
                    <td className={`${td} font-num text-xs`}>
                      {e.client}→{e.queue}
                    </td>
                    <td className={`${td} font-num text-xs text-ink-3`}>
                      {e.meta.slug}
                      {e.hold != null && (
                        <span className="ml-2 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-amber-400">
                          {e.hold ? t('spool.heldReason', { reason: e.hold }) : t('spool.held')}
                        </span>
                      )}
                      {e.hold_on_box != null && (
                        <span
                          title="Held on the box after delivery"
                          className="ml-2 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-amber-400"
                        >
                          {e.hold_on_box
                            ? t('spool.heldReason', { reason: e.hold_on_box })
                            : t('spool.held')}
                        </span>
                      )}
                    </td>
                    <td className={`${td} font-num text-xs text-ink-4`}>
                      {shortTs(e.submitted_at)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  )
}
