export type Status = 'queued' | 'running' | 'success' | 'failed' | 'skipped' | 'cancelled'

export interface Repo {
  id: number
  project_id: number
  project_name: string
  name: string
  url: string
  created_at: number
}

export interface Project {
  id: number
  name: string
  created_at: number
  repos: Repo[]
}

export interface Run {
  id: number
  repo_id: number
  repo_name: string
  project_id: number
  project_name: string
  git_ref: string
  sha: string | null
  status: Status
  trigger: string
  /** `.moloko.yml` or the GitHub workflow's name. */
  workflow: string | null
  error: string | null
  created_at: number
  started_at: number | null
  finished_at: number | null
}

export interface SecretInfo {
  name: string
  updated_at: number
}

export interface PipelineInput {
  description?: string
  required?: boolean
  default?: string | boolean | number
  type?: 'string' | 'boolean' | 'choice' | 'number' | 'environment'
  options?: string[]
}

export interface Pipeline {
  file: string
  name: string
  inputs: Record<string, PipelineInput>
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
  /** The step a running job is on. */
  step: string | null
}

export interface AgentRow {
  agent: { id: number; name: string; labels: string[]; created_at: number; last_seen: number | null }
  online: boolean
  presence: { hostname: string; os: string; version: string; connected_at: number } | null
}

export interface AgentSecret {
  id: number
  token: string
  /** WireGuard join code; null when the server runs without WireGuard. */
  join: string | null
}

export interface Images {
  registry: string
  repositories: { name: string; tags: { tag: string; digest: string; updated_at: number }[] }[]
}

/** Something runners fetch on first use: a runner image or the Windows SDK. */
export interface Download {
  id: string
  name: string
  detail: string
  state: 'ready' | 'missing' | 'downloading'
  /** The latest output line while downloading, or the error of the last attempt. */
  message: string | null
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
  return res.status === 202 || res.status === 204 ? (undefined as T) : res.json()
}

export const api = {
  projects: () => request<Project[]>('GET', '/api/projects'),
  createProject: (name: string) => request<{ id: number }>('POST', '/api/projects', { name }),
  renameProject: (id: number, name: string) => request<void>('PATCH', `/api/projects/${id}`, { name }),
  deleteProject: (id: number) => request<void>('DELETE', `/api/projects/${id}`),
  createRepo: (projectId: number, url: string, name?: string) =>
    request<{ id: number }>('POST', `/api/projects/${projectId}/repos`, { url, name: name || undefined }),
  updateRepo: (id: number, patch: { name?: string; url?: string }) => request<void>('PATCH', `/api/repos/${id}`, patch),
  deleteRepo: (id: number) => request<void>('DELETE', `/api/repos/${id}`),
  /** Write-only: lists names, never values. `scope` is `projects/<id>` or `repos/<id>`. */
  secrets: (scope: string) => request<SecretInfo[]>('GET', `/api/${scope}/secrets`),
  putSecret: (scope: string, name: string, value: string) =>
    request<void>('PUT', `/api/${scope}/secrets/${encodeURIComponent(name)}`, { value }),
  deleteSecret: (scope: string, name: string) => request<void>('DELETE', `/api/${scope}/secrets/${encodeURIComponent(name)}`),
  /** `filter` is `p<id>` for a project or `r<id>` for a repo. */
  runs: (filter?: string) => {
    const q = filter?.startsWith('p') ? `?project_id=${filter.slice(1)}` : filter?.startsWith('r') ? `?repo_id=${filter.slice(1)}` : ''
    return request<(Run & { jobs: Job[] })[]>('GET', `/api/runs${q}`)
  },
  run: (id: number) => request<{ run: Run; jobs: Job[] }>('GET', `/api/runs/${id}`),
  trigger: (repoId: number, ref: string, workflow?: string, inputs?: Record<string, string | boolean>) =>
    request<{ id: number }>('POST', `/api/repos/${repoId}/runs`, { ref, workflow, inputs }),
  /** Pipelines a manual run can start at `ref`, with their workflow_dispatch inputs. */
  pipelines: (repoId: number, ref: string) =>
    request<Pipeline[]>('GET', `/api/repos/${repoId}/workflows?ref=${encodeURIComponent(ref || 'HEAD')}`),
  cancel: (id: number) => request<void>('POST', `/api/runs/${id}/cancel`),
  agents: () => request<AgentRow[]>('GET', '/api/agents'),
  createAgent: (name: string, labels: string[]) =>
    request<AgentSecret>('POST', '/api/agents', { name, labels }),
  rotateJoin: (id: number) => request<AgentSecret>('POST', `/api/agents/${id}/join`),
  images: () => request<Images>('GET', '/api/images'),
  downloads: () => request<Download[]>('GET', '/api/downloads'),
  startDownload: (id: string) => request<void>('POST', '/api/downloads', { id }),
  deleteAgent: (id: number) => request<void>('DELETE', `/api/agents/${id}`),
}

/** "2 running · 1 queued · 3 passed", in that fixed order, leaving out empty groups. */
export function jobCounts(jobs: Job[]): string {
  const labels: [Status, string][] = [['running', 'running'], ['queued', 'queued'], ['failed', 'failed'], ['success', 'passed'], ['cancelled', 'cancelled'], ['skipped', 'skipped']]
  return labels
    .map(([s, label]) => [jobs.filter((j) => j.status === s).length, label] as const)
    .filter(([n]) => n > 0)
    .map(([n, label]) => `${n} ${label}`)
    .join(' · ')
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
