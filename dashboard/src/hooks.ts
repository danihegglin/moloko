import { useEffect, useState } from 'react'

export type Route =
  | { page: 'runs' }
  | { page: 'run'; id: number; job?: number }
  | { page: 'projects' }
  | { page: 'agents' }
  | { page: 'images' }
  | { page: 'settings' }

const CHANGE = 'moloko:route'

/** Go to another page without reloading. `replace` swaps the current history entry instead of adding one. */
export function navigate(path: string, replace = false) {
  history[replace ? 'replaceState' : 'pushState'](null, '', path)
  if (!replace) scrollTo(0, 0)
  dispatchEvent(new Event(CHANGE))
}

// Routes: / (runs), /runs/:id, /runs/:id/:job, /projects, /images, /deployments, /settings
export function useRoute(): Route {
  const [path, setPath] = useState(location.pathname)
  useEffect(() => {
    // Addresses used to be `/#/projects`; keep old bookmarks working.
    if (location.hash.startsWith('#/')) history.replaceState(null, '', location.hash.slice(1))
    const on = () => setPath(location.pathname)
    on()
    // Plain clicks on links to dashboard pages change the page in place.
    const click = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return
      const a = (e.target as Element).closest?.('a[href]') as HTMLAnchorElement | null
      if (!a || a.target || a.origin !== location.origin || a.pathname.startsWith('/api/')) return
      e.preventDefault()
      navigate(a.pathname + a.search)
    }
    addEventListener('popstate', on)
    addEventListener(CHANGE, on)
    document.addEventListener('click', click)
    return () => {
      removeEventListener('popstate', on)
      removeEventListener(CHANGE, on)
      document.removeEventListener('click', click)
    }
  }, [])
  const m = path.match(/^\/runs\/(\d+)(?:\/(\d+))?/)
  if (m) return { page: 'run', id: Number(m[1]), job: m[2] ? Number(m[2]) : undefined }
  if (path.startsWith('/projects')) return { page: 'projects' }
  if (path.startsWith('/deployments') || path.startsWith('/agents')) return { page: 'agents' }
  if (path.startsWith('/images')) return { page: 'images' }
  if (path.startsWith('/settings')) return { page: 'settings' }
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
