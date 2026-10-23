import type { RoomDetail, Snapshot } from './types'

export function getToken(): string | null {
  return new URLSearchParams(window.location.search).get('token')
}

export function wsUrl(): string {
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const token = getToken()
  const q = token === null ? '' : `?token=${encodeURIComponent(token)}`
  return `${proto}//${window.location.host}/ws${q}`
}

export async function fetchRoom(name: string, token: string | null): Promise<RoomDetail> {
  const q = token === null ? '' : `?token=${encodeURIComponent(token)}`
  const res = await fetch(`/api/room/${encodeURIComponent(name)}${q}`)
  if (!res.ok) throw new Error(`room fetch: HTTP ${res.status}`)
  return (await res.json()) as RoomDetail
}

export async function fetchLogs(
  client: string,
  queue: string | null,
  pattern: string | null,
  tail: number,
): Promise<{ ok: boolean; text?: string; error?: string }> {
  const q = queue ? `/${encodeURIComponent(queue)}` : '/'
  const u = new URL(`/api/logs/${encodeURIComponent(client)}${q}`, location.origin)
  if (pattern) u.searchParams.set('pattern', pattern)
  u.searchParams.set('tail', String(tail))
  const token = getToken()
  if (token !== null) u.searchParams.set('token', token)
  const r = await fetch(u)
  return (await r.json()) as { ok: boolean; text?: string; error?: string }
}

/** The one non-GET call: `client exec` passthrough for read-only
 *  probes. Token rides the header — keeps it out of the URL (and
 *  therefore out of server logs and Referer headers). */
export async function postExec(
  client: string,
  cmd: string,
): Promise<{ ok: boolean; out?: string; error?: string }> {
  const token = getToken()
  const r = await fetch('/api/exec', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(token !== null ? { 'x-shunt-token': token } : {}),
    },
    body: JSON.stringify({ client, cmd }),
  })
  return (await r.json()) as { ok: boolean; out?: string; error?: string }
}

/** Parse a ws text frame; returns null for anything that isn't a snapshot. */
export function parseSnapshot(text: string): Snapshot | null {
  try {
    const obj = JSON.parse(text) as { type?: unknown }
    return obj && obj.type === 'snapshot' ? (obj as unknown as Snapshot) : null
  } catch {
    return null
  }
}
