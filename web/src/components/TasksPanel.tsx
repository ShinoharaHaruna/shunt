import type { TaskRecord } from '../types'
import { hhmm } from '../fmt'
import { Card, CopyText, Empty, Pill, SectionHeading } from './ui'

const th = 'pb-2.5 pr-5 text-left text-[11px] font-medium uppercase tracking-[0.08em] text-ink-4'
const td = 'py-2 pr-5 text-sm text-ink-2'

function OutcomeBadge({ t }: { t: TaskRecord }) {
  if (!t.outcome) return <span className="text-ink-4">—</span>
  const ok = t.outcome.state === 'done'
  return (
    <Pill tone={ok ? 'emerald' : 'rose'} title={t.outcome.observed_at}>
      <span className="font-num">
        {t.outcome.state} rc={t.outcome.rc}
      </span>
    </Pill>
  )
}

export function TasksPanel({ tasks }: { tasks: { entries?: TaskRecord[]; error?: string } }) {
  const entries = tasks.entries
  return (
    <div>
      <SectionHeading title="Task ledger" count={entries?.length} />
      <Card className="p-0">
        {tasks.error ? (
          <Empty>tasks error: {tasks.error}</Empty>
        ) : !entries ? (
          <Empty>waiting for first sweep</Empty>
        ) : entries.length === 0 ? (
          <Empty>no tasks yet</Empty>
        ) : (
          <div className="overflow-x-auto px-5 py-3">
            <table className="w-full border-collapse">
              <thead>
                <tr className="border-b border-edge-soft">
                  <th className={th}>queue_name</th>
                  <th className={th}>project</th>
                  <th className={th}>route</th>
                  <th className={th}>pushed</th>
                  <th className={th}>outcome</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((t) => (
                  <tr
                    key={t.id}
                    className="border-b border-edge-soft transition-colors last:border-0 hover:bg-hover"
                  >
                    <td className={`${td} text-xs`}>
                      <CopyText text={t.queue_name} />
                    </td>
                    <td className={td}>{t.project}</td>
                    <td className={`${td} font-num text-xs`}>
                      {t.client}/{t.queue}
                    </td>
                    <td className={`${td} font-num text-xs text-ink-4`}>{hhmm(t.pushed_at)}</td>
                    <td className={td}>
                      <OutcomeBadge t={t} />
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
