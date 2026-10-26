// Wire contract for `shunt serve` — snapshot pushes over /ws, room detail over /api.

export interface Snapshot {
  type: 'snapshot'
  protocol: number
  server: string
  ts: string | null
  clients: Record<string, ClientStatus> | null
  local: LocalSection | null
}

export interface LocalSection {
  spool: { entries?: SpoolEntry[]; error?: string }
  tasks: { entries?: TaskRecord[]; error?: string }
  rooms: Listings | { error: string }
}

export interface ClientStatus {
  reachable: boolean
  queues: Record<string, QueueStatus>
  report?: StatusReport | null
  error?: string | null
  billing?: { hourly: number; currency: string } | null
  expire?: string | null
  expires_in_s?: number | null
  connect_failures?: number | null
  endpoint_rotates: boolean
  facts?: Record<string, unknown> | null
  /** Present when a background sweep chose not to touch the row: the
   *  card reports the registry's own data and `queues` is empty
   *  because nobody asked (see ProbeSkip). */
  skip?: ProbeSkip | null
}

/** Why a background sweep skipped a client — mirrors the server's
 *  `ProbeSkip` (`expired` | `backoff` | `gave_up`). */
export interface ProbeSkip {
  reason: 'expired' | 'backoff' | 'gave_up'
  fails?: number | null
  retry_in_s?: number | null
}

export interface QueueStatus {
  qroot: string
  reachable: boolean
  report?: StatusReport | null
  error?: string | null
  compat?: string | null
}

export interface StatusReport {
  protocol: number
  daemon: { alive: boolean; pid: number | null; ver: string | null; bin: string | null }
  workers: { value: number; source: string }
  running: string[]
  pending: string[]
  done_recent: ExitRecord[]
  failed_recent: ExitRecord[]
  stop: boolean
  shutdown: { armed: boolean; grace_s: number; cmd: string }
  resources?: {
    gpus: {
      index: number
      uuid: string
      name: string
      mem_total_mb: number | null
      mem_free_mb: number | null
      util_pct: number | null
    }[]
    ram_total_mb: number | null
    ram_free_mb: number | null
    load1: number | null
  } | null
  pending_detail: EntryDetail[]
  running_detail: EntryDetail[]
  needs_glibc?: string | null
}

export interface EntryDetail {
  name: string
  declared: Declared
  usage: TaskUsage | null
  held?: boolean
  hold_reason?: string | null
}

export interface Declared {
  gpu_mem_mb?: number
  ram_mb?: number
  cpu_cores?: number
  retry?: number
}

export interface TaskUsage {
  peak_vram_mb: Record<string, number>
  peak_ram_mb: number
  cpu_seconds: number
  samples: number
}

export interface ExitRecord {
  name: string
  rc: number
  usage: TaskUsage | null
  declared: Declared
}

export interface SpoolEntry {
  id: string
  project: string
  client: string
  queue: string
  qroot?: string | null
  hold?: string | null
  hold_on_box?: string | null
  meta: { slug: string; [k: string]: unknown }
  submitted_at: string
}

export interface TaskRecord {
  id: string
  project: string
  client: string
  queue: string
  qroot?: string | null
  slug: string
  queue_name: string
  submitted_at: string
  pushed_at: string
  outcome?: { state: string; rc: number; observed_at: string } | null
}

export interface Listings {
  label: string
  rooms: RoomView[]
  suggested: { endpoint: string; clients: string[]; projects: string[] }[]
  archived: string[]
}

export interface RoomView {
  name: string
  created_at: string
  members: string[]
  stale: string[]
  clients: string[]
  msgs: number
  unread_msgs?: number | null
  decision?: { ts: string; author: string; note: string } | null
}

export interface RoomMsg {
  n: number
  author: string
  ts: string
  body_md: string
  member: boolean
  summary_through: number | null
}

export interface RoomDetail {
  room: RoomView
  msgs: RoomMsg[]
}
