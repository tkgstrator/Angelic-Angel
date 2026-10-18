import { vi } from 'vitest'
import type {
  Account,
  CredentialCheck,
  FollowingNotifications,
  NotificationFilter,
  NotificationItem,
  NotificationPage,
  Registration,
  Settings,
  Status,
} from '../api'

export type Handler = (init: RequestInit, url: string) => Response | Promise<Response>

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

export function apiError(status: number, code: string, message: string): Response {
  return json({ error: { code, message } }, status)
}

/** Routes by "METHOD /path". Unknown routes fail the test loudly. */
export function mockApi(routes: Record<string, Handler>) {
  const fn = vi.fn(async (url: string, init: RequestInit = {}) => {
    const key = `${init.method ?? 'GET'} ${url}`
    const handler = routes[key]
    if (!handler) throw new Error(`unexpected request: ${key}`)
    return handler(init, url)
  })
  vi.stubGlobal('fetch', fn)
  return fn
}

export const allowAll: NotificationFilter = {
  tweet: true,
  retweet: true,
  quote: true,
  quote_self: true,
  reply: true,
  xchat_message_received: true,
  other: true,
}

export const filterOf = (over: Partial<NotificationFilter> = {}): NotificationFilter => ({
  ...allowAll,
  ...over,
})

export const settings = (over: Partial<Settings> = {}): Settings => ({
  machine_post_enabled: false,
  machine_like_enabled: false,
  machine_follow_enabled: false,
  configured: true,
  auth_token_set: true,
  ct0_set: true,
  webhook_secret_set: true,
  webhook_endpoint: 'https://example.com/hook',
  webhook_source: 'config',
  credentials_locked: false,
  notification_filter: allowAll,
  ...over,
})

export const account = (id: string, over: Partial<Account> = {}): Account => ({
  id,
  registered: true,
  receiver_state: 'connected',
  ...over,
})

export const registration = (over: Partial<Registration> = {}): Registration => ({
  registered: false,
  uaid: null,
  channel_id: null,
  ...over,
})

export const status = (over: Partial<Status> = {}): Status => ({
  state: 'connected',
  registered: true,
  connected_since: 1_700_000_000,
  last_notification_at: null,
  received_count: 0,
  forwarded_count: 0,
  failed_count: 0,
  retry_count: 0,
  last_error: null,
  ...over,
})

export const following = (over: Partial<FollowingNotifications> = {}): FollowingNotifications => ({
  state: 'idle',
  total: null,
  success_count: 0,
  failed_count: 0,
  remaining_count: null,
  last_error: null,
  ...over,
})

export const notification = (
  id: string,
  over: Partial<NotificationItem> = {},
): NotificationItem => ({
  id,
  received_at: 1_700_000_000,
  title: `title ${id}`,
  body: `body ${id}`,
  url: null,
  icon: null,
  payload: { id },
  ...over,
})

export const pageOf = (
  items: NotificationItem[],
  next_cursor: string | null = null,
  notification_filter: NotificationFilter = allowAll,
): NotificationPage => ({ items, next_cursor, notification_filter })

export const page = (
  ids: string[],
  next_cursor: string | null = null,
  notification_filter: NotificationFilter = allowAll,
): NotificationPage =>
  pageOf(
    ids.map((id) => notification(id)),
    next_cursor,
    notification_filter,
  )

export const validation = (over: Partial<CredentialCheck> = {}): CredentialCheck => ({
  valid: true,
  screen_name: 'alice',
  message: 'ok',
  ...over,
})

/** Every request the Settings screen makes on its own, answered with quiet defaults. */
export const settingsRoutes = (): Record<string, Handler> => ({
  'GET /api/accounts/default/settings': () => json(settings()),
  'GET /api/accounts/default/registration': () => json(registration()),
  'GET /api/accounts/default/following-notifications': () => json(following()),
  'POST /api/accounts/default/settings/validate': () => json(validation()),
})

export function requests(fetchMock: ReturnType<typeof mockApi>, key: string): number {
  return fetchMock.mock.calls.filter(([url, init]) => `${init?.method ?? 'GET'} ${url}` === key)
    .length
}
