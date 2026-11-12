export type Status = 'queued' | 'running' | 'success' | 'failed' | 'skipped' | 'cancelled'

export interface Project {
  id: number
  name: string
  repo_url: string
  created_at: number
}

export interface Run {
  id: number
  project_id: number
  project_name: string
  git_ref: string
  sha: string | null
  status: Status
  trigger: string
  error: string | null
  created_at: number
  started_at: number | null
  finished_at: number | null
}

export interface Job {
  id: number
  run_id: number
  name: string
  needs: string[]
  status: Status
  exit_code: number | null
  started_at: number | null
  finished_at: number | null
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  })
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: res.statusText }))
    throw new Error(err.error ?? res.statusText)
  }
  return res.status === 202 ? (undefined as T) : res.json()
}

export const api = {
  projects: () => request<Project[]>('GET', '/api/projects'),
  createProject: (name: string, repo_url: string) =>
    request<{ id: number }>('POST', '/api/projects', { name, repo_url }),
  runs: (projectId?: number) =>
    request<Run[]>('GET', `/api/runs${projectId ? `?project_id=${projectId}` : ''}`),
  run: (id: number) => request<{ run: Run; jobs: Job[] }>('GET', `/api/runs/${id}`),
  trigger: (projectId: number, ref: string) =>
    request<{ id: number }>('POST', `/api/projects/${projectId}/runs`, { ref }),
  cancel: (id: number) => request<void>('POST', `/api/runs/${id}/cancel`),
}

export const isActive = (s: Status) => s === 'queued' || s === 'running'

export function duration(start: number | null, end: number | null, now = Date.now()): string {
  if (!start) return ''
  const s = Math.max(0, Math.round(((end ?? now) - start) / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  return m < 60 ? `${m}m ${s % 60}s` : `${Math.floor(m / 60)}h ${m % 60}m`
}

export function ago(ts: number, now = Date.now()): string {
  const s = Math.round((now - ts) / 1000)
  if (s < 60) return 'just now'
  if (s < 3600) return `${Math.floor(s / 60)}m ago`
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`
  return new Date(ts).toLocaleDateString()
}

export function shortRef(ref: string) {
  return ref.replace(/^refs\/(heads|tags)\//, '')
}
