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
  ['string instead of boolean', { ...allowAll, other: 'true' }],
  ['number instead of boolean', { ...allowAll, tweet: 1 }],
  ['null value', { ...allowAll, xchat_message_received: null }],
] as const

describe('notificationFilterSchema', () => {
  it('accepts all boolean combinations', () => {
    for (const tweet of [true, false]) {
      for (const xchat_message_received of [true, false]) {
        for (const other of [true, false]) {
          const filter = { tweet, xchat_message_received, other }
          expect(notificationFilterSchema.parse(filter)).toEqual(filter)
        }
      }
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
    const filter = { tweet: false, xchat_message_received: true, other: false }
    mockApi({
      'GET /api/settings': () => json(settings({ notification_filter: filter })),
      [FIRST]: () => json(page(['a'], null, filter)),
    })
    expect((await api.getSettings()).notification_filter).toEqual(filter)
    expect((await api.getNotifications()).notification_filter).toEqual(filter)
  })
})
