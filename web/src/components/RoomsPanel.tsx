import { useEffect, useRef, useState } from 'react'
import type { CSSProperties } from 'react'
import type { Listings, RoomDetail, RoomView } from '../types'
import { fetchRoom, getToken } from '../api'
import { msgTs, shortTs } from '../fmt'
import { Card, Empty, Muted, Pill, SectionHeading } from './ui'
import { Md } from './Md'
import { useI18n } from '../i18n'

/** Stable author color — members stay visually distinct across the
 *  stream without a palette table. Hue inline, sat/lig come from
 *  the active theme's `--author-*` tokens (see `.author`). */
function authorHue(name: string): number {
  let h = 0
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) % 360
  return h
}

function RoomRow({ room, active }: { room: RoomView; active: boolean }) {
  const { t } = useI18n()
  const unread = room.unread_msgs ?? 0
  return (
    <a
      href={active ? '#/rooms' : `#/rooms/${encodeURIComponent(room.name)}`}
      className={`relative block w-full rounded-xl border px-3 py-2.5 pl-4 text-left transition-all duration-200 ${
        active
          ? 'border-edge bg-hover-2 before:absolute before:left-0 before:top-2.5 before:bottom-2.5 before:w-[2px] before:rounded-full before:bg-signal'
          : 'border-transparent hover:bg-hover-2'
      }`}
    >
      <div className="flex items-center gap-2">
        <span className="font-num truncate text-[13px] text-ink-2">{room.name}</span>
        <Muted className="text-[11px]">{t('common.members', { n: room.members.length })}</Muted>
        <span className="flex-1" />
        {room.stale.length > 0 && (
          <Pill title={t('rooms.stale', { names: room.stale.join(', ') })}>
            {t('rooms.staleCount', { n: room.stale.length })}
          </Pill>
        )}
        {unread > 0 && (
          <Pill tone="amber">
            <span className="font-num">{unread}</span>
          </Pill>
        )}
      </div>
      {room.decision && (
        <p className="mt-1 truncate text-[11px] text-ink-4" title={room.decision.note}>
          ⓘ {room.decision.note}
        </p>
      )}
    </a>
  )
}

function RoomStream({ detail }: { detail: RoomDetail }) {
  const { t } = useI18n()
  const box = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const el = box.current
    if (el) el.scrollTop = el.scrollHeight
  }, [detail.msgs.length, detail.room.name])

  if (detail.msgs.length === 0) return <Empty>{t('rooms.noMessages')}</Empty>
  return (
    <div
      ref={box}
      className="max-h-[28rem] space-y-1 overflow-y-auto pr-1 [mask-image:linear-gradient(to_bottom,transparent,black_1.5rem)]"
    >
      {detail.msgs.map((m) =>
        m.summary_through != null ? (
          <div
            key={m.n}
            className="rounded-xl border border-amber-400/20 bg-amber-400/[0.05] px-3 py-2"
          >
            <div className="mb-1 flex items-center gap-2 text-[11px]">
              <Pill tone="amber">{t('rooms.digestThrough', { n: m.summary_through })}</Pill>
              <span className="font-num text-ink-4">#{m.n}</span>
              <span className="text-ink-4">{m.author}</span>
              <span className="font-num ml-auto text-ink-4">{msgTs(m.ts)}</span>
            </div>
            <Md text={m.body_md} className="text-xs leading-relaxed text-ink-2" />
          </div>
        ) : (
          <div key={m.n} className="rounded-lg px-2 py-1.5 transition-colors hover:bg-hover">
            <div className="flex items-baseline gap-2 text-[11px]">
              <span className="font-num text-ink-4">#{m.n}</span>
              <span
                className="author font-medium"
                style={{ '--author-h': authorHue(m.author) } as CSSProperties}
              >
                {m.author}
              </span>
              {!m.member && <Pill tone="amber">{t('rooms.nonMember')}</Pill>}
              <span className="font-num ml-auto text-ink-4">{msgTs(m.ts)}</span>
            </div>
            <Md text={m.body_md} className="mt-0.5 text-[13px] leading-relaxed text-ink-2" />
          </div>
        ),
      )}
    </div>
  )
}

export function RoomsPanel({
  rooms,
  snapAt,
  selected = null,
}: {
  rooms: Listings | { error: string }
  snapAt: number
  selected?: string | null
}) {
  const { t } = useI18n()
  const [detail, setDetail] = useState<RoomDetail | null>(null)
  const [err, setErr] = useState<string | null>(null)

  useEffect(() => {
    if (!selected) {
      setDetail(null)
      return
    }
    let cancelled = false
    fetchRoom(selected, getToken())
      .then((d) => {
        if (!cancelled) {
          setDetail(d)
          setErr(null)
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) setErr(e instanceof Error ? e.message : String(e))
      })
    return () => {
      cancelled = true
    }
  }, [selected, snapAt])

  if ('error' in rooms) {
    return (
      <div>
        <SectionHeading title={t('nav.rooms')} />
        <Card>
          <Empty>{t('rooms.error', { err: rooms.error })}</Empty>
        </Card>
      </div>
    )
  }

  const activeRoom = selected && detail && detail.room.name === selected ? detail : null

  return (
    <div>
      <SectionHeading
        title={t('nav.rooms')}
        right={<Muted className="text-xs">{rooms.label}</Muted>}
      />
      <Card>
        <div className="grid gap-5 md:grid-cols-[16rem_1fr]">
          <div className="space-y-1">
            {rooms.rooms.length === 0 && <Muted className="text-[13px]">{t('rooms.none')}</Muted>}
            {rooms.rooms.map((r) => (
              <RoomRow key={r.name} room={r} active={selected === r.name} />
            ))}

            {rooms.suggested.length > 0 && (
              <div className="pt-3">
                <Muted className="text-[11px] uppercase tracking-wider">
                  {t('rooms.suggested')}
                </Muted>
                <ul className="mt-1.5 space-y-1.5">
                  {rooms.suggested.map((s) => (
                    <li key={s.endpoint} className="text-[11px] text-ink-4">
                      <span className="font-num text-ink-3">{s.endpoint}</span>
                      {s.projects.length > 0 && <span> · {s.projects.join(', ')}</span>}
                      {s.clients.length > 0 && (
                        <span className="text-ink-4"> · {s.clients.join(', ')}</span>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {rooms.archived.length > 0 && (
              <p className="pt-2 text-[11px] text-ink-4">
                {t('rooms.archived', { names: rooms.archived.join(', ') })}
              </p>
            )}
          </div>

          <div className="min-w-0">
            {!selected ? (
              <Empty>{t('rooms.select')}</Empty>
            ) : err ? (
              <Empty>{err}</Empty>
            ) : !activeRoom ? (
              <Empty>{t('state.loading')}</Empty>
            ) : (
              <div>
                <div className="mb-2 flex items-baseline gap-2">
                  <span className="font-num text-sm font-medium text-ink-2">
                    {activeRoom.room.name}
                  </span>
                  <Muted className="text-[11px]">
                    {t('rooms.stats', {
                      msgs: activeRoom.room.msgs,
                      ts: shortTs(activeRoom.room.created_at),
                    })}
                  </Muted>
                </div>
                {activeRoom.room.decision && (
                  <div className="mb-3 flex items-baseline gap-2 rounded-lg border border-signal-bd bg-signal-bg px-3 py-2">
                    <span className="eyebrow shrink-0 text-signal-fg">{t('rooms.decision')}</span>
                    <span className="text-xs text-ink-2">{activeRoom.room.decision.note}</span>
                    <Muted className="font-num ml-auto shrink-0 text-[10px]">
                      {activeRoom.room.decision.author} · {msgTs(activeRoom.room.decision.ts)}
                    </Muted>
                  </div>
                )}
                <RoomStream detail={activeRoom} />
              </div>
            )}
          </div>
        </div>
      </Card>
    </div>
  )
}
