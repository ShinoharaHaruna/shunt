import { useEffect, useState } from 'react'
import type { Snapshot } from './types'
import { parseSnapshot, wsUrl } from './api'
import { parseHash } from './nav'
import type { Route } from './nav'
import { hhmm } from './fmt'
import { Card, Dot, Empty, SectionHeading } from './components/ui'
import { ClientCard } from './components/ClientCard'
import { QueuesPage } from './components/QueuesPage'
import { SpoolPanel } from './components/SpoolPanel'
import { TasksPanel } from './components/TasksPanel'
import { RoomsPanel } from './components/RoomsPanel'

type Conn = 'connecting' | 'open' | 'reconnecting'
type Theme = 'system' | 'light' | 'dark'

const connMeta: Record<Conn, { tone: 'signal' | 'amber' | 'rose'; label: string }> = {
  connecting: { tone: 'amber', label: 'connecting' },
  open: { tone: 'signal', label: 'live' },
  reconnecting: { tone: 'amber', label: 'reconnecting' },
}

const THEME_KEY = 'shunt-theme'

function useTheme(): [Theme, (t: Theme) => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    const v = document.documentElement.dataset.theme
    return v === 'light' || v === 'dark' || v === 'system' ? v : 'system'
  })
  useEffect(() => {
    document.documentElement.dataset.theme = theme
    try {
      localStorage.setItem(THEME_KEY, theme)
    } catch {
      /* private mode — theme still applies for the session */
    }
  }, [theme])
  return [theme, setTheme]
}

function ThemeSwitch({ theme, setTheme }: { theme: Theme; setTheme: (t: Theme) => void }) {
  const opts: Theme[] = ['system', 'light', 'dark']
  return (
    <div
      className="flex items-center rounded-full border border-edge bg-hover p-0.5"
      role="group"
      aria-label="theme"
    >
      {opts.map((t) => (
        <button
          key={t}
          onClick={() => setTheme(t)}
          aria-pressed={theme === t}
          className={`rounded-full px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide transition-all duration-200 ${
            theme === t
              ? 'bg-[var(--seg-active)] text-ink-1 shadow-sm'
              : 'text-ink-3 hover:text-ink-2'
          }`}
        >
          {t === 'system' ? 'auto' : t}
        </button>
      ))}
    </div>
  )
}

function useSnapshot(): { snap: Snapshot | null; snapAt: number; conn: Conn } {
  const [snap, setSnap] = useState<Snapshot | null>(null)
  const [snapAt, setSnapAt] = useState(0)
  const [conn, setConn] = useState<Conn>('connecting')

  useEffect(() => {
    let ws: WebSocket | null = null
    let timer: number | undefined
    let attempts = 0
    let dead = false

    const schedule = () => {
      const delay = Math.min(1000 * 2 ** attempts, 10_000)

      attempts += 1
      timer = window.setTimeout(connect, delay)
    }

    const connect = () => {
      if (dead) return
      setConn(attempts === 0 ? 'connecting' : 'reconnecting')
      try {
        ws = new WebSocket(wsUrl())
      } catch {
        schedule()
        return
      }
      ws.onopen = () => {
        attempts = 0
        setConn('open')
      }
      ws.onmessage = (ev) => {
        if (typeof ev.data !== 'string') return
        const s = parseSnapshot(ev.data)
        if (!s) return
        setSnap((prev) => ({
          ...s,
          clients: s.clients ?? prev?.clients ?? null,
          local: s.local ?? prev?.local ?? null,
        }))
        setSnapAt(Date.now())
      }
      ws.onerror = () => ws?.close()
      ws.onclose = () => {
        if (!dead) {
          setConn('reconnecting')
          schedule()
        }
      }
    }

    connect()
    return () => {
      dead = true
      if (timer !== undefined) window.clearTimeout(timer)
      ws?.close()
    }
  }, [])

  return { snap, snapAt, conn }
}

function useHashRoute(): Route {
  const [route, setRoute] = useState<Route>(() => parseHash(location.hash))
  useEffect(() => {
    const on = () => setRoute(parseHash(location.hash))
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

function NavItem({
  label,
  active,
  badge,
  badgeTone = 'zinc',
  href,
}: {
  label: string
  active: boolean
  badge?: number
  badgeTone?: 'zinc' | 'amber'
  href: string
}) {
  return (
    <a
      href={href}
      className={`relative flex items-center gap-2 rounded-lg px-3 py-1.5 text-[13px] transition-colors duration-150 ${
        active
          ? 'bg-hover-2 text-ink-1 before:absolute before:left-0 before:top-1.5 before:bottom-1.5 before:w-[2px] before:rounded-full before:bg-signal'
          : 'text-ink-3 hover:bg-hover hover:text-ink-2'
      }`}
    >
      {label}
      {badge != null && badge > 0 && (
        <span
          key={badge}
          className={`badge-pop font-num ml-auto rounded-full px-1.5 text-[10px] leading-4 ${
            badgeTone === 'amber' ? 'pill-amber border' : 'bg-hover-2 text-ink-3'
          }`}
        >
          {badge}
        </span>
      )}
    </a>
  )
}

function FleetStat({
  n,
  label,
  accent,
}: {
  n: number
  label: string
  accent?: 'signal' | 'amber'
}) {
  return (
    <div>
      <span
        className="num-display block text-[28px] leading-none"
        style={{
          color:
            accent === 'signal'
              ? 'var(--signal-fg)'
              : accent === 'amber'
                ? 'var(--c-amber)'
                : 'var(--ink-1)',
        }}
      >
        {n}
      </span>
      <span className="mt-1.5 block text-[10px] font-medium uppercase tracking-[0.14em] text-ink-4">
        {label}
      </span>
    </div>
  )
}

function MachinesSection({ clients }: { clients: Snapshot['clients'] }) {
  const count = clients ? Object.keys(clients).length : 0
  const rows = Object.values(clients ?? {})
  const reachable = rows.filter((c) => c.reachable).length
  const running = rows.reduce(
    (n, c) =>
      n +
      Object.values(c.queues ?? {}).reduce(
        (m, q) => m + (q.report?.running_detail?.length ?? q.report?.running?.length ?? 0),
        0,
      ),
    0,
  )
  const pending = rows.reduce(
    (n, c) =>
      n +
      Object.values(c.queues ?? {}).reduce(
        (m, q) => m + (q.report?.pending_detail?.length ?? q.report?.pending?.length ?? 0),
        0,
      ),
    0,
  )
  return (
    <div>
      <SectionHeading title="Machines" count={count || undefined} />
      {count > 0 && (
        <div className="mb-6 flex items-end gap-8">
          {count - reachable > 0 ? (
            <FleetStat n={count - reachable} label="down" accent="amber" />
          ) : (
            <FleetStat n={reachable} label="up" accent="signal" />
          )}
          <FleetStat n={running} label="running" />
          <FleetStat n={pending} label="pending" accent={pending > 0 ? 'amber' : undefined} />
        </div>
      )}
      {clients === null ? (
        <Card>
          <Empty>waiting for first sweep</Empty>
        </Card>
      ) : count === 0 ? (
        <Card>
          <Empty>no clients registered</Empty>
        </Card>
      ) : (
        <div className="grid gap-4 xl:grid-cols-2">
          {Object.entries(clients).map(([name, c]) => (
            <ClientCard key={name} name={name} client={c} />
          ))}
        </div>
      )}
    </div>
  )
}

function WaitCard() {
  return (
    <Card>
      <Empty>waiting for first sweep</Empty>
    </Card>
  )
}

export default function App() {
  const { snap, snapAt, conn } = useSnapshot()
  const [theme, setTheme] = useTheme()
  const route = useHashRoute()
  const cm = connMeta[conn]
  const local = snap?.local ?? null

  const rooms = local && !('error' in local.rooms) ? local.rooms : null
  const unreadTotal = rooms?.rooms.reduce((n, r) => n + (r.unread_msgs ?? 0), 0) ?? 0
  const spoolCount = local?.spool.entries?.length ?? 0
  const runningTotal = Object.values(snap?.clients ?? {}).reduce(
    (n, c) =>
      n +
      Object.values(c.queues ?? {}).reduce(
        (m, q) => m + (q.report?.running_detail?.length ?? q.report?.running?.length ?? 0),
        0,
      ),
    0,
  )

  const page = (() => {
    switch (route.page) {
      case 'queues':
        return (
          <QueuesPage
            clients={snap?.clients ?? null}
            snapAt={snapAt}
            sel={route.client && route.queue ? { client: route.client, queue: route.queue } : null}
          />
        )
      case 'tasks':
        return local === null ? (
          <WaitCard />
        ) : (
          <div className="space-y-10">
            <SpoolPanel spool={local.spool} />
            <TasksPanel tasks={local.tasks} />
          </div>
        )
      case 'rooms':
        return local === null ? (
          <WaitCard />
        ) : (
          <RoomsPanel rooms={local.rooms} snapAt={snapAt} selected={route.room} />
        )
      default:
        return <MachinesSection clients={snap?.clients ?? null} />
    }
  })()

  return (
    <div className="flex min-h-screen">
      {/* ambient glow — one accent wash bleeding off the top edge;
          the only non-functional element on the page */}
      <div
        aria-hidden
        className="pointer-events-none fixed inset-x-0 top-0 h-72"
        style={{ background: 'radial-gradient(60% 100% at 50% 0%, var(--glow), transparent)' }}
      />

      {/* sidebar — desktop */}
      <aside className="sticky top-0 hidden h-screen w-52 shrink-0 flex-col border-r border-edge-soft px-3 py-5 md:flex">
        <div className="flex items-baseline gap-2 px-3">
          <span className="text-[15px] font-semibold tracking-tight text-ink-1">
            shunt<span className="text-signal-fg">.</span>
          </span>
          <span className="font-num text-[10px] text-ink-4">
            {snap?.server?.split(' ')[0] ?? ''}
          </span>
        </div>
        <nav className="mt-6 space-y-0.5">
          <NavItem label="Machines" href="#/machines" active={route.page === 'machines'} />
          <NavItem
            label="Queues"
            href="#/queues"
            active={route.page === 'queues'}
            badge={runningTotal}
          />
          <NavItem
            label="Tasks"
            href="#/tasks"
            active={route.page === 'tasks'}
            badge={spoolCount}
          />
          <NavItem
            label="Rooms"
            href="#/rooms"
            active={route.page === 'rooms'}
            badge={unreadTotal}
            badgeTone="amber"
          />
        </nav>
        <div className="mt-auto space-y-2 px-3">
          <div className="flex items-center gap-1.5 text-[11px] text-ink-3">
            <Dot tone={cm.tone} pulse={conn === 'open'} />
            {cm.label}
            {snap?.ts && <span className="font-num ml-auto text-ink-4">{hhmm(snap.ts)}</span>}
          </div>
          <ThemeSwitch theme={theme} setTheme={setTheme} />
        </div>
      </aside>

      <div className="min-w-0 flex-1">
        {/* top bar — mobile only: wordmark + conn + theme, then tabs */}
        <header
          className="sticky top-0 z-10 border-b border-edge-soft backdrop-blur-xl md:hidden"
          style={{ background: 'var(--header-bg)' }}
        >
          <div className="flex items-center gap-3 px-4 py-2.5">
            <span className="text-[15px] font-semibold tracking-tight text-ink-1">
              shunt<span className="text-signal-fg">.</span>
            </span>
            <span className="flex-1" />
            <span className="flex items-center gap-1.5 text-[11px] text-ink-3">
              <Dot tone={cm.tone} pulse={conn === 'open'} />
              {cm.label}
            </span>
            <ThemeSwitch theme={theme} setTheme={setTheme} />
          </div>
          <nav className="flex gap-1 px-4 pb-2">
            <NavItem label="Machines" href="#/machines" active={route.page === 'machines'} />
            <NavItem
              label="Queues"
              href="#/queues"
              active={route.page === 'queues'}
              badge={runningTotal}
            />
            <NavItem
              label="Tasks"
              href="#/tasks"
              active={route.page === 'tasks'}
              badge={spoolCount}
            />
            <NavItem
              label="Rooms"
              href="#/rooms"
              active={route.page === 'rooms'}
              badge={unreadTotal}
              badgeTone="amber"
            />
          </nav>
        </header>

        <main className="relative mx-auto max-w-6xl px-5 py-8 md:px-8">
          {/* the heartbeat — one hairline sweep per snapshot tick */}
          <div
            key={snapAt}
            className="heartbeat absolute inset-x-5 top-0 md:inset-x-8"
            aria-hidden
          />
          <div key={route.page} className="page-in">
            {page}
          </div>
        </main>
      </div>
    </div>
  )
}
