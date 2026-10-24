import type { SpoolEntry } from '../types'
import { shortTs } from '../fmt'
import { Card, CopyText, Empty, SectionHeading } from './ui'

const th = 'pb-2.5 pr-5 text-left text-[11px] font-medium uppercase tracking-[0.08em] text-ink-4'
const td = 'py-2 pr-5 text-sm text-ink-2'

export function SpoolPanel({ spool }: { spool: { entries?: SpoolEntry[]; error?: string } }) {
  const entries = spool.entries
  return (
    <div>
      <SectionHeading title="Spool" count={entries?.length} />
      <Card className="p-0">
        {spool.error ? (
          <Empty>spool error: {spool.error}</Empty>
        ) : !entries ? (
          <Empty>waiting for first sweep</Empty>
        ) : entries.length === 0 ? (
          <Empty>spool empty</Empty>
        ) : (
          <div className="overflow-x-auto px-5 py-3">
            <table className="w-full border-collapse">
              <thead>
                <tr className="border-b border-edge-soft">
                  <th className={th}>id</th>
                  <th className={th}>project</th>
                  <th className={th}>route</th>
                  <th className={th}>slug</th>
                  <th className={th}>submitted</th>
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
                          held{e.hold ? `: ${e.hold}` : ''}
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
