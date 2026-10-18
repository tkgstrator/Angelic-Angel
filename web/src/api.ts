import * as z from 'zod/mini'

const MAX_MESSAGE = 200

// ---- types (HTTP contract) ----

export type WebhookSource = 'environment' | 'config' | 'none'

export const notificationFilterSchema = z.object({
  tweet: z.boolean(),
  retweet: z.boolean(),
  quote: z.boolean(),
  quote_self: z.boolean(),
  reply: z.boolean(),
  xchat_message_received: z.boolean(),
  other: z.boolean(),
})

export type NotificationFilter = z.infer<typeof notificationFilterSchema>

export interface Settings {
  machine_post_enabled: boolean
  machine_like_enabled: boolean
  machine_follow_enabled: boolean
  configured: boolean
  auth_token_set: boolean
  ct0_set: boolean
  webhook_secret_set: boolean
  webhook_endpoint: string | null
  webhook_source: WebhookSource
  credentials_locked: boolean
  notification_filter: NotificationFilter
}

export interface SettingsUpdate {
  machine_post_enabled?: boolean
  machine_like_enabled?: boolean
  machine_follow_enabled?: boolean
  auth_token?: string | null
  ct0?: string | null
  webhook_secret?: string | null
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

export const receiverStateSchema = z.enum([
  'unconfigured',
  'unregistered',
  'stopped',
  'connecting',
  'connected',
  'retrying',
  'registering',
  'unregistering',
  'error',
])

export type ReceiverState = z.infer<typeof receiverStateSchema>

/** The server enforces the same rule; it is also what a safe URL segment and file name need. */
export const ACCOUNT_ID_PATTERN = /^[a-z0-9][a-z0-9_-]{0,31}$/

export const accountSchema = z.object({
  id: z.string().check(z.minLength(1)),
  registered: z.boolean(),
  receiver_state: receiverStateSchema,
})

export type Account = z.infer<typeof accountSchema>

const accountListSchema = z.object({ accounts: z.array(accountSchema) })

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

async function send(method: string, path: string, body?: unknown): Promise<Response> {
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
  return res
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  verify?: (value: T) => T,
): Promise<T> {
  const res = await send(method, path, body)
  try {
    const value = (await res.json()) as T
    return verify ? verify(value) : value
  } catch {
    throw new ApiError(res.status, 'bad_response', fallbackMessage(500))
  }
}

/** For calls whose success is the status alone: the body, if any, is not read. */
async function requestEmpty(method: string, path: string): Promise<void> {
  await send(method, path)
}

export const NOTIFICATIONS_PAGE_SIZE = 50

const accountPath = (accountId: string) => `/api/accounts/${encodeURIComponent(accountId)}`

function notificationsPath(accountId: string, before?: string): string {
  const query = new URLSearchParams()
  if (before !== undefined) query.set('before', before)
  query.set('limit', String(NOTIFICATIONS_PAGE_SIZE))
  return `${accountPath(accountId)}/notifications?${query}`
}

function verifyFilter<T extends { notification_filter: NotificationFilter }>(value: T): T {
  return {
    ...value,
    notification_filter: notificationFilterSchema.parse(value.notification_filter),
  }
}

function verifySettings(value: Settings): Settings {
  return {
    ...verifyFilter(value),
    machine_post_enabled: z.boolean().parse(value.machine_post_enabled),
    machine_like_enabled: z.boolean().parse(value.machine_like_enabled),
    machine_follow_enabled: z.boolean().parse(value.machine_follow_enabled),
  }
}

export const api = {
  listAccounts: async () => {
    const list = await request<{ accounts: Account[] }>('GET', '/api/accounts', undefined, (v) =>
      accountListSchema.parse(v),
    )
    return list.accounts
  },
  createAccount: (id: string) =>
    request<Account>('POST', '/api/accounts', { id }, (v) => accountSchema.parse(v)),
  deleteAccount: (accountId: string) => requestEmpty('DELETE', accountPath(accountId)),
  getSettings: (accountId: string) =>
    request<Settings>('GET', `${accountPath(accountId)}/settings`, undefined, verifySettings),
  putSettings: (accountId: string, update: SettingsUpdate) =>
    request<Settings>('PUT', `${accountPath(accountId)}/settings`, update, verifySettings),
  validateSettings: (accountId: string) =>
    request<CredentialCheck>('POST', `${accountPath(accountId)}/settings/validate`),
  getRegistration: (accountId: string) =>
    request<Registration>('GET', `${accountPath(accountId)}/registration`),
  register: (accountId: string) =>
    request<Registration>('POST', `${accountPath(accountId)}/register`),
  unregister: (accountId: string) =>
    request<Registration>('DELETE', `${accountPath(accountId)}/registration`),
  getStatus: (accountId: string) => request<Status>('GET', `${accountPath(accountId)}/status`),
  getFollowingNotifications: (accountId: string) =>
    request<FollowingNotifications>('GET', `${accountPath(accountId)}/following-notifications`),
  startFollowingNotifications: (accountId: string) =>
    request<FollowingNotifications>('POST', `${accountPath(accountId)}/following-notifications`),
  cancelFollowingNotifications: (accountId: string) =>
    request<FollowingNotifications>('DELETE', `${accountPath(accountId)}/following-notifications`),
  getNotifications: (accountId: string, before?: string) =>
    request<NotificationPage>('GET', notificationsPath(accountId, before), undefined, verifyFilter),
}
