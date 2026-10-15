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
