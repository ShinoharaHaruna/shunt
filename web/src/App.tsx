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
import { LANGS, useI18n } from './i18n'
import type { Lang } from './i18n'

type Conn = 'connecting' | 'open' | 'reconnecting' | 'down'
type Theme = 'system' | 'light' | 'dark'

const connTone: Record<Conn, 'signal' | 'amber' | 'rose'> = {
  connecting: 'amber',
  open: 'signal',
  reconnecting: 'amber',
  down: 'rose',
}

/** Automatic reconnect budget. `serve` writes a new listener within a
 *  second of USR2 (or a couple on a restart), so five attempts cover
 *  every recoverable case; past that the port is not coming back on
 *  its own and the page says so instead of hammering forever. */
const MAX_RECONNECT_ATTEMPTS = 5

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
  const { t } = useI18n()
  const opts: Theme[] = ['system', 'light', 'dark']
  return (
    <div
      className="flex items-center rounded-full border border-edge bg-hover p-0.5"
      role="group"
      aria-label={t('theme.aria')}
    >
      {opts.map((o) => (
        <button
          key={o}
          onClick={() => setTheme(o)}
          aria-pressed={theme === o}
          className={`shrink-0 whitespace-nowrap rounded-full px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide transition-all duration-200 ${
            theme === o
              ? 'bg-[var(--seg-active)] text-ink-1 shadow-sm'
              : 'text-ink-3 hover:text-ink-2'
          }`}
        >
          {t(`theme.${o}`)}
        </button>
      ))}
    </div>
  )
}

/** Globe icon + hover popover — a segmented switch grows with every
 *  language added and already overflows the sidebar next to the
 *  theme switch. `dir` controls which way the popover opens:
 *  'up' for the sidebar footer, 'down' for the mobile top bar. */
function LangMenu({ dir }: { dir: 'up' | 'down' }) {
  const { lang, setLang, t } = useI18n()
  const [open, setOpen] = useState(false)
  return (
    <div
      className="relative"
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      {/* a tap fires mouseenter before click, so click only ever
          opens — toggling here would instantly undo the open */}
      <button
        onClick={() => setOpen(true)}
        aria-label={t('lang.aria')}
        aria-expanded={open}
        className={`flex h-6 w-6 items-center justify-center rounded-full transition-colors ${
          open ? 'bg-hover-2 text-ink-1' : 'text-ink-3 hover:bg-hover-2 hover:text-ink-2'
        }`}
      >
        <svg
          viewBox="0 0 16 16"
          className="h-3.5 w-3.5"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.2"
          aria-hidden
        >
          <circle cx="8" cy="8" r="6.4" />
          <path d="M1.6 8h12.8" />
          <path d="M8 1.6c-1.9 1.9-2.9 4-2.9 6.4s1 4.5 2.9 6.4c1.9-1.9 2.9-4 2.9-6.4S9.9 3.5 8 1.6z" />
        </svg>
      </button>
      {/* tap-outside to close — hover users get mouseleave instead */}
      {open && <div className="fixed inset-0 z-10" onClick={() => setOpen(false)} aria-hidden />}
      {/* the offset is padding on the hoverable wrapper, not margin —
          a margin gap would close the menu before the pointer arrives */}
      {open && (
        <div
          className={`absolute z-20 ${dir === 'up' ? 'bottom-full left-0 pb-1.5' : 'right-0 top-full pt-1.5'}`}
        >
          <div
            role="menu"
            className="min-w-28 overflow-hidden rounded-xl border border-edge py-1 shadow-lg backdrop-blur-xl"
            style={{ background: 'var(--header-bg)' }}
          >
            {(Object.keys(LANGS) as Lang[]).map((l) => (
              <button
                key={l}
                role="menuitemradio"
                aria-checked={lang === l}
                onClick={() => {
                  setLang(l)
                  setOpen(false)
                }}
                className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-[12px] transition-colors ${
                  lang === l ? 'text-ink-1' : 'text-ink-3 hover:bg-hover-2 hover:text-ink-2'
                }`}
              >
                <span
                  className={`w-3 text-[10px] ${lang === l ? 'text-signal-fg' : 'text-transparent'}`}
                >
                  ✓
                </span>
                {LANGS[l]}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

function useSnapshot(): { snap: Snapshot | null; snapAt: number; conn: Conn; retry: () => void } {
  const [snap, setSnap] = useState<Snapshot | null>(null)
  const [snapAt, setSnapAt] = useState(0)
  const [conn, setConn] = useState<Conn>('connecting')
  // A manual retry re-runs the effect, which is where the attempt
  // budget lives — clicking resets it rather than resuming a spent one.
  const [nonce, setNonce] = useState(0)

  useEffect(() => {
    let ws: WebSocket | null = null
    let timer: number | undefined
    let attempts = 0
    let dead = false

    const connect = () => {
      if (dead) return
      setConn(attempts === 0 ? 'connecting' : 'reconnecting')
      try {
        ws = new WebSocket(wsUrl())
      } catch {
        attempts += 1
        if (attempts >= MAX_RECONNECT_ATTEMPTS) setConn('down')
        else timer = window.setTimeout(connect, 1000 * 2 ** (attempts - 1))
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
        if (dead) return
        // Each failure doubles the wait (1, 2, 4, 8 s) and the fifth
        // stops the loop: a stale tab must not poll a dead endpoint
        // forever — the last snapshot stays on screen, marked offline.
        attempts += 1
        if (attempts >= MAX_RECONNECT_ATTEMPTS) {
          setConn('down')
          return
        }
        setConn('reconnecting')
        timer = window.setTimeout(connect, 1000 * 2 ** (attempts - 1))
      }
    }

    connect()
    return () => {
      dead = true
      if (timer !== undefined) window.clearTimeout(timer)
      ws?.close()
    }
  }, [nonce])

  return { snap, snapAt, conn, retry: () => setNonce((n) => n + 1) }
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

/** Connection state, and the escape hatch: once the reconnect budget
 *  is spent the state names itself and offers a fresh attempt — a
 *  `serve` that is gone stays gone until something changes. */
function ConnBadge({ conn, retry, ts }: { conn: Conn; retry: () => void; ts?: string | null }) {
  const { t } = useI18n()
  return (
    <span className="flex items-center gap-1.5 text-[11px] text-ink-3">
      <Dot tone={connTone[conn]} pulse={conn === 'open'} />
      {conn === 'down' ? (
        <>
          <span className="text-ink-4">{t('conn.down')}</span>
          <button
            onClick={retry}
            className="rounded-full border border-edge px-1.5 py-0.5 font-medium text-ink-2 transition-colors hover:bg-hover-2 hover:text-ink-1"
          >
            {t('conn.retry')}
          </button>
        </>
      ) : (
        t(conn === 'open' ? 'conn.live' : `conn.${conn}`)
      )}
      {ts && <span className="font-num ml-auto text-ink-4">{hhmm(ts)}</span>}
    </span>
  )
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
  const { t } = useI18n()
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
      <SectionHeading title={t('nav.machines')} count={count || undefined} />
      {count > 0 && (
        <div className="mb-6 flex items-end gap-8">
          {count - reachable > 0 ? (
            <FleetStat n={count - reachable} label={t('machines.down')} accent="amber" />
          ) : (
            <FleetStat n={reachable} label={t('machines.up')} accent="signal" />
          )}
          <FleetStat n={running} label={t('machines.running')} />
          <FleetStat
            n={pending}
            label={t('machines.pending')}
            accent={pending > 0 ? 'amber' : undefined}
          />
        </div>
      )}
      {clients === null ? (
        <Card>
          <Empty>{t('state.waitingSweep')}</Empty>
        </Card>
      ) : count === 0 ? (
        <Card>
          <Empty>{t('machines.noClients')}</Empty>
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
  const { t } = useI18n()
  return (
    <Card>
      <Empty>{t('state.waitingSweep')}</Empty>
    </Card>
  )
}

export default function App() {
  const { t } = useI18n()
  const { snap, snapAt, conn, retry } = useSnapshot()
  const [theme, setTheme] = useTheme()
  const route = useHashRoute()
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
          <NavItem label={t('nav.machines')} href="#/machines" active={route.page === 'machines'} />
          <NavItem
            label={t('nav.queues')}
            href="#/queues"
            active={route.page === 'queues'}
            badge={runningTotal}
          />
          <NavItem
            label={t('nav.tasks')}
            href="#/tasks"
            active={route.page === 'tasks'}
            badge={spoolCount}
          />
          <NavItem
            label={t('nav.rooms')}
            href="#/rooms"
            active={route.page === 'rooms'}
            badge={unreadTotal}
            badgeTone="amber"
          />
        </nav>
        <div className="mt-auto space-y-2 px-3">
          <ConnBadge conn={conn} retry={retry} ts={snap?.ts} />
          <div className="flex items-center gap-2">
            <ThemeSwitch theme={theme} setTheme={setTheme} />
            <LangMenu dir="up" />
          </div>
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
            <ConnBadge conn={conn} retry={retry} />
            <LangMenu dir="down" />
            <ThemeSwitch theme={theme} setTheme={setTheme} />
          </div>
          <nav className="flex gap-1 px-4 pb-2">
            <NavItem
              label={t('nav.machines')}
              href="#/machines"
              active={route.page === 'machines'}
            />
            <NavItem
              label={t('nav.queues')}
              href="#/queues"
              active={route.page === 'queues'}
              badge={runningTotal}
            />
            <NavItem
              label={t('nav.tasks')}
              href="#/tasks"
              active={route.page === 'tasks'}
              badge={spoolCount}
            />
            <NavItem
              label={t('nav.rooms')}
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
