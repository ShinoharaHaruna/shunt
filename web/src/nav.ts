export type Page = 'machines' | 'queues' | 'tasks' | 'rooms'

export interface Route {
  page: Page
  /** '#/queues/<client>/<queue>' selection. */
  client?: string
  queue?: string
  /** '#/rooms/<name>' selection. */
  room?: string
}

export function parseHash(hash: string): Route {
  const parts = hash.replace(/^#\/?/, '').split('/').filter(Boolean).map(decodeURIComponent)
  switch (parts[0]) {
    case 'queues':
      return { page: 'queues', client: parts[1], queue: parts[2] }
    case 'tasks':
      return { page: 'tasks' }
    case 'rooms':
      return { page: 'rooms', room: parts[1] }
    default:
      return { page: 'machines' }
  }
}
