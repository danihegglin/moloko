import { useEffect, useState } from 'react'

/** The hash route split into path parts and query, e.g. #/p/1/reviews/new?repo=2 → ['p','1','reviews','new']. */
export function useRoute() {
  const [hash, setHash] = useState(location.hash)
  useEffect(() => {
    const on = () => setHash(location.hash)
    addEventListener('hashchange', on)
    return () => removeEventListener('hashchange', on)
  }, [])
  const [path, query = ''] = hash.replace(/^#\/?/, '').split('?')
  return { parts: path.split('/').filter(Boolean).map(decodeURIComponent), query: new URLSearchParams(query) }
}

export const go = (hash: string) => {
  location.hash = hash
}

/** Poll `load` every `ms` (and immediately); stops polling while `active` is false. */
export function usePoll<T>(load: () => Promise<T>, ms: number, deps: unknown[], active: (v: T) => boolean = () => true) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [version, setVersion] = useState(0)
  useEffect(() => {
    let stop = false
    let timer: ReturnType<typeof setTimeout>
    const tick = async () => {
      try {
        const v = await load()
        if (stop) return
        setData(v)
        setError(null)
        if (active(v)) timer = setTimeout(tick, ms)
      } catch (e) {
        if (stop) return
        setError(String((e as Error).message ?? e))
        timer = setTimeout(tick, ms * 2)
      }
    }
    tick()
    return () => {
      stop = true
      clearTimeout(timer)
    }
  }, [...deps, version])
  return { data, error, reload: () => setVersion((v) => v + 1) }
}

export function useNow(ms = 1000) {
  const [now, setNow] = useState(Date.now())
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms)
    return () => clearInterval(t)
  }, [ms])
  return now
}

/** Run an async action, keeping its error for display. */
export function useAction() {
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const run = async (f: () => Promise<unknown>) => {
    setBusy(true)
    setError(null)
    try {
      await f()
      return true
    } catch (e) {
      setError((e as Error).message)
      return false
    } finally {
      setBusy(false)
    }
  }
  return { error, busy, run }
}
