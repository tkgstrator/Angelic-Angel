import { describe, expect, it } from 'vitest'
import { ApiError, api, notificationFilterSchema } from './api'
import { allowAll, json, mockApi, page, settings } from './test/helpers'

const FIRST = 'GET /api/accounts/default/notifications?limit=50'

const { quote_self: _quoteSelf, ...legacyFilter } = allowAll

const invalid = [
  ['missing', undefined],
  ['null', null],
  ['not an object', 'all'],
  ['empty object', {}],
  ['missing key', { tweet: true, other: true }],
  ['missing retweet', { tweet: true, xchat_message_received: true, other: true }],
  ['missing quote and reply', { ...allowAll, quote: undefined, reply: undefined }],
  ['missing quote', { ...allowAll, quote: undefined }],
  ['missing quote_self', { ...allowAll, quote_self: undefined }],
  ['a filter from before quote_self existed', legacyFilter],
  ['missing reply', { ...allowAll, reply: undefined }],
  ['string instead of boolean', { ...allowAll, other: 'true' }],
  ['number instead of boolean', { ...allowAll, tweet: 1 }],
  ['null value', { ...allowAll, xchat_message_received: null }],
  ['null retweet', { ...allowAll, retweet: null }],
  ['string retweet', { ...allowAll, retweet: 'false' }],
  ['null quote', { ...allowAll, quote: null }],
  ['string quote', { ...allowAll, quote: 'false' }],
  ['null quote_self', { ...allowAll, quote_self: null }],
  ['string quote_self', { ...allowAll, quote_self: 'false' }],
  ['null reply', { ...allowAll, reply: null }],
  ['number reply', { ...allowAll, reply: 0 }],
] as const

describe('notificationFilterSchema', () => {
  it('accepts all boolean combinations', () => {
    for (const bits of Array.from({ length: 128 }, (_, n) => n)) {
      const on = (bit: number) => (bits & (1 << bit)) !== 0
      const filter = {
        tweet: on(0),
        retweet: on(1),
        quote: on(2),
        quote_self: on(3),
        reply: on(4),
        xchat_message_received: on(5),
        other: on(6),
      }
      expect(notificationFilterSchema.parse(filter)).toEqual(filter)
    }
  })

  it.each(invalid)('rejects %s', (_, value) => {
    expect(notificationFilterSchema.safeParse(value).success).toBe(false)
  })
})

describe('notification_filter at the API boundary', () => {
  it.each(invalid)('getSettings rejects a response with %s', async (_, value) => {
    mockApi({
      'GET /api/accounts/default/settings': () =>
        json({ ...settings(), notification_filter: value }),
    })
    await expect(api.getSettings('default')).rejects.toMatchObject({
      name: 'ApiError',
      code: 'bad_response',
    })
  })

  it.each(invalid)('putSettings rejects a response with %s', async (_, value) => {
    mockApi({
      'PUT /api/accounts/default/settings': () =>
        json({ ...settings(), notification_filter: value }),
    })
    await expect(
      api.putSettings('default', { webhook_endpoint: '', notification_filter: allowAll }),
    ).rejects.toBeInstanceOf(ApiError)
  })

  it.each(invalid)('getNotifications rejects a page with %s', async (_, value) => {
    mockApi({ [FIRST]: () => json({ ...page(['a']), notification_filter: value }) })
    await expect(api.getNotifications('default')).rejects.toMatchObject({ code: 'bad_response' })
  })

  it('rejects a response from a server that does not know the filter yet', async () => {
    const { notification_filter: _, ...legacy } = settings()
    mockApi({ 'GET /api/accounts/default/settings': () => json(legacy) })
    await expect(api.getSettings('default')).rejects.toBeInstanceOf(ApiError)
  })

  it('rejects a response body that is not an object', async () => {
    mockApi({ [FIRST]: () => json(null) })
    await expect(api.getNotifications('default')).rejects.toMatchObject({ code: 'bad_response' })
  })

  it('passes valid filters through unchanged', async () => {
    const filter = {
      tweet: false,
      retweet: false,
      quote: false,
      quote_self: true,
      reply: true,
      xchat_message_received: true,
      other: false,
    }
    mockApi({
      'GET /api/accounts/default/settings': () => json(settings({ notification_filter: filter })),
      [FIRST]: () => json(page(['a'], null, filter)),
    })
    expect((await api.getSettings('default')).notification_filter).toEqual(filter)
    expect((await api.getNotifications('default')).notification_filter).toEqual(filter)
  })
})

describe('machine permission at the API boundary', () => {
  const KEYS = ['machine_post_enabled', 'machine_like_enabled', 'machine_follow_enabled'] as const
  const permissions = {
    machine_post_enabled: false,
    machine_like_enabled: false,
    machine_follow_enabled: false,
  }

  it.each(
    KEYS.flatMap((key) =>
      [undefined, null, 'true', 'false', 1, {}].map((value) => [key, value] as const),
    ),
  )('rejects invalid %s permission %s', async (key, value) => {
    mockApi({
      'GET /api/accounts/default/settings': () => json({ ...settings(), [key]: value }),
      'PUT /api/accounts/default/settings': () => json({ ...settings(), [key]: value }),
    })
    await expect(api.getSettings('default')).rejects.toMatchObject({ code: 'bad_response' })
    await expect(
      api.putSettings('default', {
        webhook_endpoint: '',
        notification_filter: allowAll,
        ...permissions,
      }),
    ).rejects.toMatchObject({ code: 'bad_response' })
  })

  it('rejects a response that lacks a permission key instead of assuming off', async () => {
    const { machine_like_enabled: _like, ...withoutLike } = settings()
    mockApi({ 'GET /api/accounts/default/settings': () => json(withoutLike) })
    await expect(api.getSettings('default')).rejects.toMatchObject({ code: 'bad_response' })
  })

  it('accepts and returns all three permissions as booleans', async () => {
    const saved = settings({
      machine_post_enabled: true,
      machine_like_enabled: true,
      machine_follow_enabled: false,
    })
    mockApi({
      'GET /api/accounts/default/settings': () => json(saved),
      'PUT /api/accounts/default/settings': () => json(saved),
    })
    const read = await api.getSettings('default')
    expect(read).toMatchObject({
      machine_post_enabled: true,
      machine_like_enabled: true,
      machine_follow_enabled: false,
    })
    const written = await api.putSettings('default', {
      webhook_endpoint: '',
      notification_filter: allowAll,
      machine_like_enabled: true,
    })
    expect(written.machine_like_enabled).toBe(true)
    expect(written.machine_follow_enabled).toBe(false)
  })

  it('sends only the permissions it is given, so an omitted one is retained by the server', async () => {
    let body: Record<string, unknown> = {}
    mockApi({
      'PUT /api/accounts/default/settings': (init) => {
        body = JSON.parse(String(init.body))
        return json(settings())
      },
    })
    await api.putSettings('default', {
      webhook_endpoint: '',
      notification_filter: allowAll,
      machine_follow_enabled: true,
    })
    expect(body).toMatchObject({ machine_follow_enabled: true })
    expect(body).not.toHaveProperty('machine_post_enabled')
    expect(body).not.toHaveProperty('machine_like_enabled')
  })
})
