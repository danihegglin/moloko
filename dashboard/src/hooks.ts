import { useEffect, useState } from 'react'

export type Route =
  | { page: 'runs' }
  | { page: 'run'; id: number; job?: number }
  | { page: 'projects' }
  | { page: 'agents' }
  | { page: 'images' }
  | { page: 'settings' }

// Hash routes: #/ (runs), #/runs/:id, #/runs/:id/:job, #/projects, #/agents, #/images, #/settings
export function useRoute(): Route {
  const [hash, setHash] = useState(location.hash)
  useEffect(() => {
    const on = () => setHash(location.hash)
    addEventListener('hashchange', on)
    return () => removeEventListener('hashchange', on)
  }, [])
  const m = hash.match(/^#\/runs\/(\d+)(?:\/(\d+))?/)
  if (m) return { page: 'run', id: Number(m[1]), job: m[2] ? Number(m[2]) : undefined }
  if (hash.startsWith('#/projects')) return { page: 'projects' }
  if (hash.startsWith('#/agents')) return { page: 'agents' }
  if (hash.startsWith('#/images')) return { page: 'images' }
  if (hash.startsWith('#/settings')) return { page: 'settings' }
  return { page: 'runs' }
}

/** Poll `load` every `ms` (and immediately); stops polling while `active` is false. */
export function usePoll<T>(load: () => Promise<T>, ms: number, deps: unknown[], active: (v: T) => boolean = () => true) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
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
  }, deps)
  return { data, error }
}

export function useNow(ms = 1000) {
  const [now, setNow] = useState(Date.now())
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms)
    return () => clearInterval(t)
  }, [ms])
  return now
}
