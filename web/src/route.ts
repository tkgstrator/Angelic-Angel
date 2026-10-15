import { useEffect, useState } from 'react'
import type { Account } from './api'

export type Screen = 'notifications' | 'settings' | 'status'
export const SCREENS: readonly Screen[] = ['notifications', 'settings', 'status']

const FALLBACK_SCREEN: Screen = 'settings'
const DEFAULT_ACCOUNT = 'default'

/** `account` is null when the hash names no account (an old `#/settings`, an empty or unknown hash). */
export interface Route {
  account: string | null
  screen: Screen
}

function screenOf(name: string | undefined): Screen | undefined {
  return SCREENS.find((s) => s === name)
}

function decode(segment: string): string | null {
  try {
    return decodeURIComponent(segment)
  } catch {
    return null
  }
}

/** `#/<account>/<screen>`; `#/<screen>` and anything unknown leave the account to `resolveAccount`. */
export function parseRoute(hash: string): Route {
  const parts = hash.replace(/^#\/?/, '').split('/')
  const [first = '', second] = parts
  if (parts.length === 2 && first !== '' && second !== '') {
    return { account: decode(first), screen: screenOf(second) ?? FALLBACK_SCREEN }
  }
  const legacy = parts.length === 1 ? screenOf(first) : undefined
  return { account: null, screen: legacy ?? FALLBACK_SCREEN }
}

export function screenHref(account: string | null, screen: Screen): string {
  return account === null ? `#/${screen}` : `#/${encodeURIComponent(account)}/${screen}`
}

export function useRoute(): Route {
  const [route, setRoute] = useState<Route>(() => parseRoute(window.location.hash))
  useEffect(() => {
    const on = () => setRoute(parseRoute(window.location.hash))
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

export type Resolved =
  | { kind: 'active'; account: Account }
  | { kind: 'missing'; id: string }
  | { kind: 'none' }

/**
 * Works out which account to show without touching the URL: a named account must exist,
 * otherwise the old hash forms use `default` (or the first account when there is none).
 */
export function resolveAccount(accounts: readonly Account[], requested: string | null): Resolved {
  if (accounts.length === 0) return { kind: 'none' }
  if (requested !== null) {
    const found = accounts.find((a) => a.id === requested)
    return found ? { kind: 'active', account: found } : { kind: 'missing', id: requested }
  }
  const fallback = accounts.find((a) => a.id === DEFAULT_ACCOUNT) ?? accounts[0]
  return fallback ? { kind: 'active', account: fallback } : { kind: 'none' }
}
