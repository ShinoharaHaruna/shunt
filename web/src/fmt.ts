const dtf = new Intl.DateTimeFormat(undefined, {
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

const stf = new Intl.DateTimeFormat(undefined, {
  month: 'short',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

/** "14:03" — falls back to the raw string when unparseable. */
export function hhmm(iso: string | null | undefined): string {
  if (!iso) return '—'
  const t = Date.parse(iso)
  return Number.isNaN(t) ? iso : dtf.format(t)
}

/** "Oct 24 14:03" for submitted/pushed timestamps. */
export function shortTs(iso: string | null | undefined): string {
  if (!iso) return '—'
  const t = Date.parse(iso)
  return Number.isNaN(t) ? iso : stf.format(t)
}

/** 85123 → "23h38m"; 3723 → "1h2m"; 500 → "8m". Negative → "EXPIRED" handled by caller or here. */
export function humanizeSecs(s: number): string {
  if (s < 0) return 'EXPIRED'
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (h > 0) return m > 0 ? `${h}h${m}m` : `${h}h`
  if (m > 0) return `${m}m`
  return `${s}s`
}

/** MiB → GiB, one decimal. */
export function mibToGib(mb: number): string {
  return (mb / 1024).toFixed(1)
}

/** cpu_seconds → "12m core-time" */
export function coreTime(secs: number): string {
  const m = Math.floor(secs / 60)
  return `${m}m core-time`
}
