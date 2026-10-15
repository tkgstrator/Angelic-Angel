import { describe, expect, it } from 'vitest'
import { normalizeNotificationsResponseHeaders } from '../vite.config'

function relay(method: string, url: string) {
  const proxyRes = {
    headers: {
      connection: 'close',
      'content-length': '75768',
      'content-type': 'application/json',
    },
  }
  normalizeNotificationsResponseHeaders(proxyRes, { method, url })
  return proxyRes.headers
}

describe('normalizeNotificationsResponseHeaders', () => {
  it.each([
    '/api/accounts/default/notifications?limit=50',
    '/api/accounts/default/notifications',
    '/api/accounts/my-acct_2/notifications?before=abc&limit=50',
  ])('drops content-length and connection for GET %s', (url) => {
    expect(relay('GET', url)).toEqual({ 'content-type': 'application/json' })
  })

  it.each([
    ['GET', '/api/accounts/default/settings'],
    ['GET', '/api/accounts/default/following-notifications?limit=50'],
    ['GET', '/api/accounts/default/notifications/extra'],
    ['GET', '/api/accounts/notifications'],
    ['GET', '/api/accounts'],
    ['GET', '/api/notifications'],
    ['GET', '/health'],
    ['POST', '/api/accounts/default/notifications'],
  ])('keeps content-length and connection for %s %s', (method, url) => {
    expect(relay(method, url)).toEqual({
      connection: 'close',
      'content-length': '75768',
      'content-type': 'application/json',
    })
  })
})
