import * as z from 'zod/mini'

const MAX_MESSAGE = 200

// ---- types (HTTP contract) ----

export type WebhookSource = 'environment' | 'config' | 'none'

export const notificationFilterSchema = z.object({
  tweet: z.boolean(),
  xchat_message_received: z.boolean(),
  other: z.boolean(),
})

export type NotificationFilter = z.infer<typeof notificationFilterSchema>

export interface Settings {
  configured: boolean
  auth_token_set: boolean
  ct0_set: boolean
  webhook_endpoint: string | null
  webhook_source: WebhookSource
  credentials_locked: boolean
  notification_filter: NotificationFilter
}

export interface SettingsUpdate {
  auth_token?: string | null
  ct0?: string | null
  webhook_endpoint: string
  notification_filter: NotificationFilter
}

export interface CredentialCheck {
  valid: boolean
  screen_name: string | null
  message: string
}

export interface Registration {
  registered: boolean
  uaid: string | null
  channel_id: string | null
}

export type ReceiverState =
  | 'unconfigured'
  | 'unregistered'
  | 'stopped'
  | 'connecting'
  | 'connected'
  | 'retrying'
  | 'registering'
  | 'unregistering'
  | 'error'

export interface Status {
  state: ReceiverState
  registered: boolean
  connected_since: number | null
  last_notification_at: number | null
  received_count: number
  forwarded_count: number
  failed_count: number
  retry_count: number
  last_error: string | null
}

export type FollowingNotificationsState =
  | 'idle'
  | 'loading'
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'failed'
  | 'cancelled'

export interface FollowingNotifications {
  state: FollowingNotificationsState
  total: number | null
  success_count: number
  failed_count: number
  remaining_count: number | null
  last_error: string | null
}

export interface NotificationItem {
  id: string
  received_at: number
  title: string
  body: string
  url: string | null
  icon: string | null
  payload: unknown
}

export interface NotificationPage {
  items: NotificationItem[]
  next_cursor: string | null
  notification_filter: NotificationFilter
}

// ---- errors ----

export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

function fallbackMessage(status: number): string {
  switch (status) {
    case 0:
      return 'サーバーに接続できません。'
    case 400:
      return '入力内容が正しくありません。'
    case 429:
      return 'リクエストが多すぎます。しばらくしてから再試行してください。'
    case 409:
      return '他の操作と競合しました。しばらくしてから再試行してください。'
    case 502:
      return '外部サービスでエラーが発生しました。'
    default:
      return 'サーバーでエラーが発生しました。'
  }
}

export function cleanMessage(value: unknown): string | null {
  if (typeof value !== 'string') return null
  const text = value.replace(/[\u0000-\u001f\u007f]/g, ' ').trim()
  if (text === '' || text.length > MAX_MESSAGE) return null
  return text
}

async function toError(res: Response): Promise<ApiError> {
  let code = 'unknown'
  let message: string | null = null
  try {
    const body: unknown = await res.json()
    const err = (body as { error?: { code?: unknown; message?: unknown } } | null)?.error
    if (typeof err?.code === 'string' && /^[a-z0-9_]{1,64}$/.test(err.code)) code = err.code
    message = cleanMessage(err?.message)
  } catch {
    // non-JSON body: use the generic message
  }
  return new ApiError(res.status, code, message ?? fallbackMessage(res.status))
}

export function errorText(e: unknown): string {
  return e instanceof ApiError ? e.message : fallbackMessage(500)
}

// ---- transport ----

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  verify?: (value: T) => T,
): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (body !== undefined) headers['Content-Type'] = 'application/json'

  let res: Response
  try {
    res = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: 'no-store',
      credentials: 'omit',
    })
  } catch {
    throw new ApiError(0, 'network', fallbackMessage(0))
  }
  if (!res.ok) throw await toError(res)
  try {
    const value = (await res.json()) as T
    return verify ? verify(value) : value
  } catch {
    throw new ApiError(res.status, 'bad_response', fallbackMessage(500))
  }
}

export const NOTIFICATIONS_PAGE_SIZE = 50

function notificationsPath(before?: string): string {
  const query = new URLSearchParams()
  if (before !== undefined) query.set('before', before)
  query.set('limit', String(NOTIFICATIONS_PAGE_SIZE))
  return `/api/notifications?${query}`
}

function verifyFilter<T extends { notification_filter: NotificationFilter }>(value: T): T {
  return {
    ...value,
    notification_filter: notificationFilterSchema.parse(value.notification_filter),
  }
}

export const api = {
  getSettings: () => request<Settings>('GET', '/api/settings', undefined, verifyFilter),
  putSettings: (update: SettingsUpdate) =>
    request<Settings>('PUT', '/api/settings', update, verifyFilter),
  validateSettings: () => request<CredentialCheck>('POST', '/api/settings/validate'),
  getRegistration: () => request<Registration>('GET', '/api/registration'),
  register: () => request<Registration>('POST', '/api/register'),
  unregister: () => request<Registration>('DELETE', '/api/registration'),
  getStatus: () => request<Status>('GET', '/api/status'),
  getFollowingNotifications: () =>
    request<FollowingNotifications>('GET', '/api/following-notifications'),
  startFollowingNotifications: () =>
    request<FollowingNotifications>('POST', '/api/following-notifications'),
  cancelFollowingNotifications: () =>
    request<FollowingNotifications>('DELETE', '/api/following-notifications'),
  getNotifications: (before?: string) =>
    request<NotificationPage>('GET', notificationsPath(before), undefined, verifyFilter),
}
