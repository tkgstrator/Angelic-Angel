import { describe, expect, it } from 'vitest'
import { ApiError, api, notificationFilterSchema } from './api'
import { allowAll, json, mockApi, page, settings } from './test/helpers'

const FIRST = 'GET /api/notifications?limit=50'

const invalid = [
  ['missing', undefined],
  ['null', null],
  ['not an object', 'all'],
  ['empty object', {}],
  ['missing key', { tweet: true, other: true }],
  ['missing retweet', { tweet: true, xchat_message_received: true, other: true }],
  ['missing quote and reply', { ...allowAll, quote: undefined, reply: undefined }],
  ['missing quote', { ...allowAll, quote: undefined }],
  ['missing reply', { ...allowAll, reply: undefined }],
  ['string instead of boolean', { ...allowAll, other: 'true' }],
  ['number instead of boolean', { ...allowAll, tweet: 1 }],
  ['null value', { ...allowAll, xchat_message_received: null }],
  ['null retweet', { ...allowAll, retweet: null }],
  ['string retweet', { ...allowAll, retweet: 'false' }],
  ['null quote', { ...allowAll, quote: null }],
  ['string quote', { ...allowAll, quote: 'false' }],
  ['null reply', { ...allowAll, reply: null }],
  ['number reply', { ...allowAll, reply: 0 }],
] as const

describe('notificationFilterSchema', () => {
  it('accepts all boolean combinations', () => {
    for (const bits of Array.from({ length: 64 }, (_, n) => n)) {
      const on = (bit: number) => (bits & (1 << bit)) !== 0
      const filter = {
        tweet: on(0),
        retweet: on(1),
        quote: on(2),
        reply: on(3),
        xchat_message_received: on(4),
        other: on(5),
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
    mockApi({ 'GET /api/settings': () => json({ ...settings(), notification_filter: value }) })
    await expect(api.getSettings()).rejects.toMatchObject({
      name: 'ApiError',
      code: 'bad_response',
    })
  })

  it.each(invalid)('putSettings rejects a response with %s', async (_, value) => {
    mockApi({ 'PUT /api/settings': () => json({ ...settings(), notification_filter: value }) })
    await expect(
      api.putSettings({ webhook_endpoint: '', notification_filter: allowAll }),
    ).rejects.toBeInstanceOf(ApiError)
  })

  it.each(invalid)('getNotifications rejects a page with %s', async (_, value) => {
    mockApi({ [FIRST]: () => json({ ...page(['a']), notification_filter: value }) })
    await expect(api.getNotifications()).rejects.toMatchObject({ code: 'bad_response' })
  })

  it('rejects a response from a server that does not know the filter yet', async () => {
    const { notification_filter: _, ...legacy } = settings()
    mockApi({ 'GET /api/settings': () => json(legacy) })
    await expect(api.getSettings()).rejects.toBeInstanceOf(ApiError)
  })

  it('rejects a response body that is not an object', async () => {
    mockApi({ [FIRST]: () => json(null) })
    await expect(api.getNotifications()).rejects.toMatchObject({ code: 'bad_response' })
  })

  it('passes valid filters through unchanged', async () => {
    const filter = {
      tweet: false,
      retweet: false,
      quote: false,
      reply: true,
      xchat_message_received: true,
      other: false,
    }
    mockApi({
      'GET /api/settings': () => json(settings({ notification_filter: filter })),
      [FIRST]: () => json(page(['a'], null, filter)),
    })
    expect((await api.getSettings()).notification_filter).toEqual(filter)
    expect((await api.getNotifications()).notification_filter).toEqual(filter)
  })
})
