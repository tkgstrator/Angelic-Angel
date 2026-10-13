import { describe, expect, it } from 'vitest'
import type { NotificationFilter } from './api'
import { sameFilter } from './notification-feed'
import { allowAll, filterOf } from './test/helpers'

const KEYS = Object.keys(allowAll) as (keyof NotificationFilter)[]

describe('sameFilter', () => {
  it('is true for equal filters', () => {
    expect(sameFilter(allowAll, filterOf())).toBe(true)
    expect(sameFilter(filterOf({ quote_self: false }), filterOf({ quote_self: false }))).toBe(true)
  })

  it.each(KEYS)('is false when only %s differs', (key) => {
    expect(sameFilter(allowAll, filterOf({ [key]: false }))).toBe(false)
    expect(sameFilter(filterOf({ [key]: false }), allowAll)).toBe(false)
  })

  it('tells quote_self and quote apart', () => {
    expect(sameFilter(filterOf({ quote_self: false }), filterOf({ quote: false }))).toBe(false)
  })

  it('knows every key of the filter', () => {
    expect(KEYS).toContain('quote_self')
    expect(KEYS).toHaveLength(7)
  })
})
