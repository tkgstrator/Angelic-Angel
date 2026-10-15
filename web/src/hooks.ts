import { useCallback, useEffect, useRef, useState } from 'react'

export interface Loaded<T> {
  data: T | null
  error: unknown
  loading: boolean
  reload: () => void
  setData: (data: T) => void
}

export function useLoad<T>(fn: () => Promise<T>): Loaded<T> {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<unknown>(null)
  const [loading, setLoading] = useState(true)
  const [nonce, setNonce] = useState(0)
  const fnRef = useRef(fn)
  fnRef.current = fn

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    fnRef
      .current()
      .then((d) => {
        if (!cancelled) setData(d)
      })
      .catch((e) => {
        if (!cancelled) setError(e)
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [nonce])

  const reload = useCallback(() => setNonce((n) => n + 1), [])
  return { data, error, loading, reload, setData }
}

export interface Exclusive {
  /** Name of the running action, or null when idle. */
  active: string | null
  /** Runs `fn` unless another action holds the lock; the lock is released when `fn` settles. */
  run: (name: string, fn: () => Promise<void>) => Promise<void>
}

/** One lock for a screen, so saving, checking, registering and bulk requests never overlap. */
export function useExclusive(): Exclusive {
  const [active, setActive] = useState<string | null>(null)
  const held = useRef(false)
  const run = useCallback(async (name: string, fn: () => Promise<void>) => {
    if (held.current) return
    held.current = true
    setActive(name)
    try {
      await fn()
    } finally {
      held.current = false
      setActive(null)
    }
  }, [])
  return { active, run }
}

/**
 * Polls while the document is visible; never starts a request while one is in flight.
 * `refresh` fetches now (a no-op while a request is in flight); `setData` publishes a fresher value.
 */
export function usePoll<T>(fn: () => Promise<T>, intervalMs: number) {
  const tickRef = useRef<() => void>(() => {})
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<unknown>(null)
  const [loaded, setLoaded] = useState(false)
  const fnRef = useRef(fn)
  fnRef.current = fn

  useEffect(() => {
    let stopped = false
    let inflight = false
    let timer: ReturnType<typeof setTimeout> | undefined

    const clear = () => {
      if (timer !== undefined) clearTimeout(timer)
      timer = undefined
    }
    const schedule = () => {
      clear()
      if (!stopped && !document.hidden) timer = setTimeout(tick, intervalMs)
    }
    async function tick() {
      clear()
      if (stopped || inflight) return
      inflight = true
      try {
        const d = await fnRef.current()
        if (!stopped) {
          setData(d)
          setError(null)
        }
      } catch (e) {
        if (!stopped) setError(e)
      } finally {
        inflight = false
        if (!stopped) setLoaded(true)
        schedule()
      }
    }
    const onVisibility = () => {
      if (document.hidden) clear()
      else void tick()
    }

    document.addEventListener('visibilitychange', onVisibility)
    tickRef.current = () => void tick()
    void tick()
    return () => {
      stopped = true
      tickRef.current = () => {}
      clear()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [intervalMs])

  const refresh = useCallback(() => tickRef.current(), [])
  return { data, error, loaded, setData, refresh }
}
