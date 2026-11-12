import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, ago, duration, isActive, shortRef, type Job, type Project, type Status } from './api'

// Hash routes: #/ (runs) and #/runs/:id
function useRoute() {
  const [hash, setHash] = useState(location.hash)
  useEffect(() => {
    const on = () => setHash(location.hash)
    addEventListener('hashchange', on)
    return () => removeEventListener('hashchange', on)
  }, [])
  const m = hash.match(/^#\/runs\/(\d+)/)
  return m ? { page: 'run' as const, id: Number(m[1]) } : { page: 'runs' as const }
}

/** Poll `load` every `ms` (and immediately); stops polling while `active` is false. */
function usePoll<T>(load: () => Promise<T>, ms: number, deps: unknown[], active: (v: T) => boolean = () => true) {
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

function useNow(ms = 1000) {
  const [now, setNow] = useState(Date.now())
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms)
    return () => clearInterval(t)
  }, [ms])
  return now
}

function Badge({ status }: { status: Status }) {
  return <span className={`badge ${status}`}>{status}</span>
}

export default function App() {
  const route = useRoute()
  return (
    <div className="shell">
      <header className="topbar">
        <a href="#/" className="logo">
          moloko
        </a>
        <span className="tagline">build server</span>
      </header>
      <main>{route.page === 'run' ? <RunPage id={route.id} /> : <RunsPage />}</main>
    </div>
  )
}

function RunsPage() {
  const [projectId, setProjectId] = useState<number | undefined>()
  const [projectsVersion, setProjectsVersion] = useState(0)
  const projects = usePoll(api.projects, 30000, [projectsVersion])
  const runs = usePoll(() => api.runs(projectId), 2000, [projectId])
  const now = useNow()

  return (
    <>
      <section className="toolbar">
        <select value={projectId ?? ''} onChange={(e) => setProjectId(e.target.value ? Number(e.target.value) : undefined)}>
          <option value="">All projects</option>
          {projects.data?.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <TriggerForm projects={projects.data ?? []} defaultProject={projectId} />
        <AddProject onAdded={() => setProjectsVersion((v) => v + 1)} />
      </section>
      {runs.error && <p className="error">{runs.error}</p>}
      {runs.data && runs.data.length === 0 && (
        <div className="empty">
          <p>No runs yet.</p>
          <p className="muted">
            Add a project, commit a <code>.moloko.yml</code> to it, then trigger a run or point a GitHub webhook at{' '}
            <code>/api/webhooks/github</code>.
          </p>
        </div>
      )}
      <table className="runs">
        <tbody>
          {runs.data?.map((r) => (
            <tr key={r.id} onClick={() => (location.hash = `#/runs/${r.id}`)}>
              <td className="num">#{r.id}</td>
              <td>
                <Badge status={r.status} />
              </td>
              <td>
                <strong>{r.project_name}</strong>
                <span className="muted"> · {shortRef(r.git_ref)}</span>
                {r.sha && <code className="sha">{r.sha.slice(0, 7)}</code>}
              </td>
              <td className="muted">{r.trigger}</td>
              <td className="muted right">{duration(r.started_at, r.finished_at, now)}</td>
              <td className="muted right">{ago(r.created_at, now)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  )
}

function TriggerForm({ projects, defaultProject }: { projects: Project[]; defaultProject?: number }) {
  const [project, setProject] = useState<number | undefined>(defaultProject)
  const [ref, setRef] = useState('')
  const [error, setError] = useState<string | null>(null)
  const selected = project ?? defaultProject ?? projects[0]?.id
  if (projects.length === 0) return null
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!selected) return
    try {
      const { id } = await api.trigger(selected, ref)
      location.hash = `#/runs/${id}`
    } catch (err) {
      setError((err as Error).message)
    }
  }
  return (
    <form className="inline" onSubmit={submit}>
      <select value={selected} onChange={(e) => setProject(Number(e.target.value))}>
        {projects.map((p) => (
          <option key={p.id} value={p.id}>
            {p.name}
          </option>
        ))}
      </select>
      <input placeholder="branch, tag or sha (default HEAD)" value={ref} onChange={(e) => setRef(e.target.value)} />
      <button type="submit">Run</button>
      {error && <span className="error">{error}</span>}
    </form>
  )
}

function AddProject({ onAdded }: { onAdded: () => void }) {
  const [open, setOpen] = useState(false)
  const [name, setName] = useState('')
  const [url, setUrl] = useState('')
  const [error, setError] = useState<string | null>(null)
  if (!open)
    return (
      <button className="secondary" onClick={() => setOpen(true)}>
        + Project
      </button>
    )
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      await api.createProject(name, url)
      setOpen(false)
      setName('')
      setUrl('')
      onAdded()
    } catch (err) {
      setError((err as Error).message)
    }
  }
  return (
    <form className="inline" onSubmit={submit}>
      <input placeholder="name" value={name} onChange={(e) => setName(e.target.value)} required />
      <input placeholder="git URL" value={url} onChange={(e) => setUrl(e.target.value)} required size={36} />
      <button type="submit">Add</button>
      <button type="button" className="secondary" onClick={() => setOpen(false)}>
        Cancel
      </button>
      {error && <span className="error">{error}</span>}
    </form>
  )
}

function RunPage({ id }: { id: number }) {
  const { data, error } = usePoll(() => api.run(id), 1000, [id], (d) => isActive(d.run.status))
  const [selected, setSelected] = useState<number | null>(null)
  const now = useNow()

  // Follow the first running (or failed) job until the user picks one.
  const jobs = data?.jobs ?? []
  const auto = jobs.find((j) => j.status === 'running') ?? jobs.find((j) => j.status === 'failed') ?? jobs[0]
  const current = jobs.find((j) => j.id === selected) ?? auto

  if (error && !data) return <p className="error">{error}</p>
  if (!data) return <p className="muted">Loading…</p>
  const { run } = data

  return (
    <>
      <section className="run-head">
        <a href="#/" className="muted">
          ← runs
        </a>
        <h1>
          {run.project_name} <span className="muted">#{run.id}</span> <Badge status={run.status} />
        </h1>
        <p className="muted">
          {shortRef(run.git_ref)} {run.sha && <code className="sha">{run.sha.slice(0, 10)}</code>} · {run.trigger} ·{' '}
          {duration(run.started_at, run.finished_at, now) || 'waiting'}
          {isActive(run.status) && (
            <button className="danger small" onClick={() => api.cancel(run.id).catch(() => {})}>
              Cancel
            </button>
          )}
        </p>
        {run.error && <pre className="run-error">{run.error}</pre>}
      </section>
      {jobs.length > 0 && <JobGraph jobs={jobs} selected={current?.id} onSelect={setSelected} now={now} />}
      {current && current.status === 'queued' && <p className="muted">{current.name} is waiting to start.</p>}
      {current && !['queued', 'skipped'].includes(current.status) && <LogView key={current.id} job={current} />}
    </>
  )
}

/** Lay jobs out in columns by dependency depth. */
function JobGraph({ jobs, selected, onSelect, now }: { jobs: Job[]; selected?: number; onSelect: (id: number) => void; now: number }) {
  const columns = useMemo(() => {
    const byName = new Map(jobs.map((j) => [j.name, j]))
    const depth = new Map<string, number>()
    const visit = (j: Job): number => {
      if (depth.has(j.name)) return depth.get(j.name)!
      const d = Math.max(-1, ...j.needs.map((n) => (byName.has(n) ? visit(byName.get(n)!) : -1))) + 1
      depth.set(j.name, d)
      return d
    }
    jobs.forEach(visit)
    const cols: Job[][] = []
    for (const j of jobs) (cols[depth.get(j.name)!] ??= []).push(j)
    return cols
  }, [jobs])

  return (
    <section className="graph">
      {columns.map((col, i) => (
        <div className="col" key={i}>
          {col.map((j) => (
            <button key={j.id} className={`job ${j.status} ${j.id === selected ? 'selected' : ''}`} onClick={() => onSelect(j.id)}>
              <span className="dot" />
              <span className="job-name">{j.name}</span>
              <span className="muted">{duration(j.started_at, j.finished_at, now)}</span>
            </button>
          ))}
        </div>
      ))}
    </section>
  )
}

const ANSI = /\x1b\[[0-9;]*[A-Za-z]/g

function LogView({ job }: { job: Job }) {
  const [lines, setLines] = useState<string[]>([])
  const [follow, setFollow] = useState(true)
  const box = useRef<HTMLDivElement>(null)
  const buffer = useRef<string[]>([])

  useEffect(() => {
    setLines([])
    buffer.current = []
    const es = new EventSource(`/api/jobs/${job.id}/logs`)
    // Batch updates per animation frame so fast logs don't re-render per line.
    let frame = 0
    const flush = () => {
      frame = 0
      const batch = buffer.current
      buffer.current = []
      setLines((prev) => prev.concat(batch))
    }
    es.addEventListener('line', (e) => {
      buffer.current.push((e as MessageEvent).data.replace(ANSI, ''))
      if (!frame) frame = requestAnimationFrame(flush)
    })
    es.addEventListener('end', () => es.close())
    return () => {
      es.close()
      cancelAnimationFrame(frame)
    }
  }, [job.id])

  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight
  }, [lines, follow])

  const onScroll = useCallback(() => {
    const el = box.current
    if (el) setFollow(el.scrollHeight - el.scrollTop - el.clientHeight < 40)
  }, [])

  return (
    <section className="logs">
      <div className="logs-head">
        <strong>{job.name}</strong> <Badge status={job.status} />
        {job.exit_code != null && job.exit_code !== 0 && <span className="muted"> exit {job.exit_code}</span>}
      </div>
      <div className="log-box" ref={box} onScroll={onScroll}>
        {lines.map((l, i) => (
          <div key={i} className={`log-line ${l.startsWith('▶') ? 'step' : l.startsWith('✗') ? 'fail' : l.startsWith('●') ? 'end' : ''}`}>
            <span className="ln">{i + 1}</span>
            <span>{l}</span>
          </div>
        ))}
      </div>
    </section>
  )
}
