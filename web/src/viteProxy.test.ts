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
  it.each(['/api/notifications?limit=50', '/api/notifications'])(
    'drops content-length and connection for GET %s',
    (url) => {
      expect(relay('GET', url)).toEqual({ 'content-type': 'application/json' })
    },
  )

  it.each([
    ['GET', '/api/settings'],
    ['GET', '/api/following-notifications?limit=50'],
    ['GET', '/api/notifications/extra'],
    ['GET', '/health'],
    ['POST', '/api/notifications'],
  ])('keeps content-length and connection for %s %s', (method, url) => {
    expect(relay(method, url)).toEqual({
      connection: 'close',
      'content-length': '75768',
      'content-type': 'application/json',
    })
  })
})
