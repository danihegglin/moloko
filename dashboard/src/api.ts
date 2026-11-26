export type Status = 'queued' | 'running' | 'success' | 'failed' | 'skipped' | 'cancelled'

export interface Project {
  id: number
  name: string
  repo_url: string
  created_at: number
  key: string
}

export interface User {
  id: number
  username: string
  display_name: string
  kind: 'human' | 'bot'
  is_admin: boolean
  created_at: number
}

export type Role = 'read' | 'write' | 'admin'
export type MirrorMode = 'none' | 'pull' | 'push' | 'twoway'

export interface Repo {
  id: number
  project_id: number
  name: string
  default_branch: string
  upstream_url: string | null
  mirror_mode: MirrorMode
  on_unreachable: 'reject' | 'queue'
  has_credential: boolean
  protect_default: boolean
  require_review: boolean
  created_at: number
}

export interface ProjectDetail {
  project: Project
  repos: Repo[]
  role: Role
  members: { user: User; role: Role }[]
}

export interface MirrorRef {
  ref: string
  local_sha: string | null
  remote_sha: string | null
  state: 'in_sync' | 'pending' | 'diverged' | 'error'
  last_error: string | null
  updated_at: number
}

export const STATUSES = ['backlog', 'todo', 'in_progress', 'in_review', 'done', 'cancelled'] as const
export type IssueStatus = (typeof STATUSES)[number]
export const TYPES = ['task', 'bug', 'story', 'epic'] as const
export const PRIORITIES = ['none', 'low', 'medium', 'high', 'urgent'] as const

export interface Issue {
  id: number
  project_id: number
  number: number
  key: string
  type: (typeof TYPES)[number]
  status: IssueStatus
  priority: (typeof PRIORITIES)[number]
  title: string
  body: string
  assignee: string | null
  assignee_kind: string | null
  reporter: string | null
  parent_id: number | null
  cycle_id: number | null
  estimate: number | null
  due: string | null
  labels: string[]
  rank: number
  closed_at: number | null
  released_at: number | null
  released_env: string | null
  created_at: number
  updated_at: number
}

export interface IssueEvent {
  id: number
  actor: string | null
  actor_kind: string | null
  kind: string
  body: string
  data: Record<string, unknown> | null
  created_at: number
}

export interface IssueRef {
  kind: 'branch' | 'commit' | 'run' | 'review' | 'merge' | 'deploy'
  repo_id: number | null
  repo: string | null
  ref: string
  sha: string | null
  data: Record<string, unknown> | null
  created_at: number
}

export interface Session {
  id: number
  agent: string
  issue_id: number | null
  review_id: number | null
  run_id: number | null
  kind: string
  instruction: string
  state: string
  branch: string | null
  result: string | null
  created_at: number
  finished_at: number | null
}

export interface IssueDetail {
  issue: Issue
  events: IssueEvent[]
  refs: IssueRef[]
  links: { kind: string; direction: 'in' | 'out'; key: string; id: number; title: string; status: IssueStatus }[]
  children: Issue[]
  parent: Issue | null
  reviews: Review[]
  sessions: Session[]
  branch_name: string
}

export interface Cycle {
  id: number
  name: string
  starts_at: number
  ends_at: number
}

export interface Review {
  id: number
  project_id: number
  repo_id: number
  repo: string
  number: number
  title: string
  body: string
  source_branch: string
  target_branch: string
  author: string | null
  author_kind: string | null
  state: 'draft' | 'open' | 'merged' | 'closed'
  head_sha: string
  merge_sha: string | null
  issue_id: number | null
  issue_key: string | null
  queue_state: 'queued' | 'testing' | 'failed' | null
  queue_run_id: number | null
  created_at: number
  updated_at: number
  merged_at: number | null
}

export interface ReviewComment {
  id: number
  parent_id: number | null
  author: string | null
  author_kind: string | null
  path: string | null
  line: number | null
  side: string | null
  sha: string | null
  body: string
  resolved: boolean
  created_at: number
}

export interface Commit {
  sha: string
  parents: string[]
  author: string
  email: string
  date: number
  subject: string
  body: string
}

export interface FileStat {
  path: string
  added: number | null
  removed: number | null
}

export interface ReviewDetail {
  review: Review
  status: {
    mergeable: boolean
    blockers: string[]
    behind: boolean
    conflicts: boolean
    checks: { state: string; run?: Run; required?: { name: string; status: Status }[] }
    approvals: { user: string; kind: string; state: string; stale: boolean }[]
    rules: { required_approvals: number; merge: string; merge_queue: boolean; ai_review: string; ai_reviewer: string | null }
  } | null
  files: FileStat[]
  diff: string
  commits: Commit[]
  comments: ReviewComment[]
  events: { kind: string; body: string; actor: string | null; created_at: number }[]
  change_set: Review[]
}

export interface TreeEntry {
  name: string
  type: 'blob' | 'tree' | 'commit'
  path: string
  size: number | null
}

export interface RefRow {
  name: string
  sha: string
  subject: string
  date: number
  author: string
  run: Run | null
  mirror: string | null
}

export interface AiAgent {
  id: number
  project_id: number
  user_id: number
  name: string
  environment: { kind: 'container' | 'vm' | 'host'; image: string; cpus?: number | null; memory?: string | null; driver?: string | null }
  command: string
  review_command: string | null
  secrets: string[]
  timeout_secs: number
  max_sessions: number
  auto_fix_ci: boolean
  created_at: number
}

export interface TokenRow {
  id: number
  name: string
  created_at: number
  expires_at: number | null
  last_used: number | null
  scoped: boolean
}

export interface Run {
  id: number
  project_id: number
  project_name: string
  repo_id: number | null
  repo_name: string | null
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

/** Called when the server says the session is gone, so the app can show the login form. */
export let onUnauthorized: () => void = () => {}
export function setOnUnauthorized(f: () => void) {
  onUnauthorized = f
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  })
  if (res.status === 401 && !path.startsWith('/api/auth/')) onUnauthorized()
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: res.statusText }))
    throw new Error(err.error ?? res.statusText)
  }
  return res.status === 202 || res.status === 204 ? (undefined as T) : res.json()
}

const q = (params: Record<string, string | number | undefined | null>) => {
  const p = Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== '')
  return p.length ? '?' + p.map(([k, v]) => `${k}=${encodeURIComponent(String(v))}`).join('&') : ''
}

export const api = {
  me: async (): Promise<{ user: User | null; setup_required: boolean }> => {
    const res = await fetch('/api/auth/me')
    const body = await res.json().catch(() => ({}))
    return { user: res.ok ? body.user : null, setup_required: !!body.setup_required }
  },
  login: (username: string, password: string) => request<{ user: User }>('POST', '/api/auth/login', { username, password }),
  setup: (username: string, password: string) => request<{ user: User }>('POST', '/api/auth/setup', { username, password }),
  logout: () => request<void>('POST', '/api/auth/logout'),
  users: () => request<User[]>('GET', '/api/users'),
  createUser: (username: string, password: string, is_admin: boolean) =>
    request<{ id: number }>('POST', '/api/users', { username, password, is_admin }),
  changePassword: (current: string, next: string) => request<void>('POST', '/api/users/me/password', { current, new: next }),
  tokens: () => request<TokenRow[]>('GET', '/api/tokens'),
  createToken: (name: string) => request<{ id: number; token: string }>('POST', '/api/tokens', { name }),
  deleteToken: (id: number) => request<void>('DELETE', `/api/tokens/${id}`),

  projects: () => request<Project[]>('GET', '/api/projects'),
  project: (id: number) => request<ProjectDetail>('GET', `/api/projects/${id}`),
  createProject: (body: { name: string; repo_url?: string; key?: string; mirror_mode?: MirrorMode; credential?: string }) =>
    request<{ id: number }>('POST', '/api/projects', body),
  updateProject: (id: number, body: { key?: string }) => request<void>('PATCH', `/api/projects/${id}`, body),
  setMember: (id: number, username: string, role: Role) => request<void>('PUT', `/api/projects/${id}/members`, { username, role }),
  removeMember: (id: number, userId: number) => request<void>('DELETE', `/api/projects/${id}/members/${userId}`),

  createRepo: (projectId: number, body: { name: string; upstream_url?: string; mirror_mode?: MirrorMode; credential?: string }) =>
    request<Repo>('POST', `/api/projects/${projectId}/repos`, body),
  repo: (id: number) => request<{ repo: Repo; clone_path: string }>('GET', `/api/repos/${id}`),
  updateRepo: (id: number, body: Partial<Omit<Repo, 'id' | 'project_id' | 'has_credential' | 'created_at' | 'name'>> & { credential?: string }) =>
    request<Repo>('PATCH', `/api/repos/${id}`, body),
  deleteRepo: (id: number) => request<void>('DELETE', `/api/repos/${id}`),
  mirror: (id: number) => request<MirrorRef[]>('GET', `/api/repos/${id}/mirror`),
  syncRepo: (id: number) => request<{ updated: string[] }>('POST', `/api/repos/${id}/sync`),
  resolveMirror: (id: number, ref: string, keep: 'moloko' | 'upstream') => request<void>('POST', `/api/repos/${id}/mirror/resolve`, { ref, keep }),
  branches: (id: number) => request<{ default_branch: string; refs: RefRow[] }>('GET', `/api/repos/${id}/branches`),
  tree: (id: number, ref?: string, path?: string) =>
    request<{ sha: string; path: string; entries: TreeEntry[]; readme: { path: string; content: string } | null }>('GET', `/api/repos/${id}/tree${q({ ref, path })}`),
  blob: (id: number, ref: string | undefined, path: string) =>
    request<{ sha: string; path: string; size: number; binary: boolean; content: string | null }>('GET', `/api/repos/${id}/blob${q({ ref, path })}`),
  commits: (id: number, ref?: string, path?: string) =>
    request<{ commit: Commit; run: Run | null }[]>('GET', `/api/repos/${id}/commits${q({ ref, path })}`),
  commit: (id: number, sha: string) => request<{ commit: Commit; files: FileStat[]; diff: string; run: Run | null }>('GET', `/api/repos/${id}/commits/${sha}`),

  issues: (projectId: number, query: string) => request<Issue[]>('GET', `/api/projects/${projectId}/issues${q({ q: query })}`),
  issueByNumber: (projectId: number, n: number) => request<IssueDetail>('GET', `/api/projects/${projectId}/issues/${n}`),
  createIssue: (projectId: number, body: Partial<Issue> & { title: string; assignee?: string }) =>
    request<Issue>('POST', `/api/projects/${projectId}/issues`, body),
  updateIssue: (id: number, body: Record<string, unknown>) => request<Issue>('PATCH', `/api/issues/${id}`, body),
  commentIssue: (id: number, body: string) => request<{ id: number }>('POST', `/api/issues/${id}/comments`, { body }),
  linkIssue: (id: number, to: string, kind: string) => request<void>('POST', `/api/issues/${id}/links`, { to, kind }),
  issueBranch: (id: number, repo_id: number) => request<{ branch: string }>('POST', `/api/issues/${id}/branch`, { repo_id }),
  startAi: (id: number, agent: string, instruction: string) => request<{ session: number }>('POST', `/api/issues/${id}/ai`, { agent, instruction }),
  cycles: (projectId: number) => request<Cycle[]>('GET', `/api/projects/${projectId}/cycles`),
  createCycle: (projectId: number, name: string, starts_at: number, ends_at: number) =>
    request<{ id: number }>('POST', `/api/projects/${projectId}/cycles`, { name, starts_at, ends_at }),
  burndown: (cycleId: number) =>
    request<{ total: number; starts_at: number; ends_at: number; points: { day: number; remaining: number }[] }>('GET', `/api/cycles/${cycleId}/burndown`),
  importIssues: (projectId: number, format: 'jira-csv' | 'github-json', data: string) =>
    request<{ created: number }>('POST', `/api/projects/${projectId}/import`, { format, data }),

  reviews: (projectId: number, state: string) => request<Review[]>('GET', `/api/projects/${projectId}/reviews${q({ state })}`),
  reviewByNumber: (projectId: number, n: number) => request<ReviewDetail>('GET', `/api/projects/${projectId}/reviews/${n}`),
  createReview: (projectId: number, body: { repo_id: number; source_branch: string; target_branch?: string; title?: string; body?: string; draft?: boolean }) =>
    request<Review>('POST', `/api/projects/${projectId}/reviews`, body),
  updateReview: (id: number, body: { title?: string; body?: string; state?: string }) => request<Review>('PATCH', `/api/reviews/${id}`, body),
  commentReview: (id: number, body: { body: string; path?: string; line?: number; side?: string; parent_id?: number }) =>
    request<{ id: number }>('POST', `/api/reviews/${id}/comments`, body),
  resolveComment: (id: number, cid: number, resolved: boolean) => request<void>('POST', `/api/reviews/${id}/comments/${cid}/resolve`, { resolved }),
  approve: (id: number, state: 'approved' | 'changes_requested', body?: string) => request<void>('POST', `/api/reviews/${id}/approve`, { state, body }),
  merge: (id: number) => request<{ result: 'merged' | 'queued' }>('POST', `/api/reviews/${id}/merge`),
  dequeue: (id: number) => request<void>('DELETE', `/api/reviews/${id}/queue`),

  aiAgents: (projectId: number) => request<AiAgent[]>('GET', `/api/projects/${projectId}/ai-agents`),
  createAiAgent: (projectId: number, body: Partial<AiAgent>) => request<AiAgent>('POST', `/api/projects/${projectId}/ai-agents`, body),
  updateAiAgent: (id: number, body: Partial<AiAgent>) => request<AiAgent>('PATCH', `/api/ai-agents/${id}`, body),
  deleteAiAgent: (id: number) => request<void>('DELETE', `/api/ai-agents/${id}`),
  secrets: (projectId: number) => request<string[]>('GET', `/api/projects/${projectId}/secrets`),
  setSecret: (projectId: number, name: string, value: string) => request<void>('PUT', `/api/projects/${projectId}/secrets/${name}`, { value }),
  deleteSecret: (projectId: number, name: string) => request<void>('DELETE', `/api/projects/${projectId}/secrets/${name}`),
  sessions: (projectId: number) => request<Session[]>('GET', `/api/projects/${projectId}/ai-sessions`),

  runs: (projectId?: number) =>
    request<Run[]>('GET', `/api/runs${projectId ? `?project_id=${projectId}` : ''}`),
  run: (id: number) => request<{ run: Run; jobs: Job[] }>('GET', `/api/runs/${id}`),
  trigger: (projectId: number, ref: string, repo_id?: number) =>
    request<{ id: number }>('POST', `/api/projects/${projectId}/runs`, { ref, repo_id }),
  cancel: (id: number) => request<void>('POST', `/api/runs/${id}/cancel`),
  agents: () => request<AgentRow[]>('GET', '/api/agents'),
  createAgent: (name: string, labels: string[]) =>
    request<AgentSecret>('POST', '/api/agents', { name, labels }),
  rotateJoin: (id: number) => request<AgentSecret>('POST', `/api/agents/${id}/join`),
  images: () => request<Images>('GET', '/api/images'),
  deleteAgent: (id: number) => request<void>('DELETE', `/api/agents/${id}`),
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
