import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ArrowDownIcon, ArrowLeftIcon } from 'lucide-react'
import { api, duration, isActive, jobCounts, shortRef, type Job } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { cn } from '@/lib/utils'
import { attempt, JobBar, Muted, StatusDot, StatusLabel } from '@/components/common'
import { Button } from '@/components/ui/button'

export function RunPage({ id, job }: { id: number; job?: number }) {
  const { data, error } = usePoll(() => api.run(id), 1000, [id], (d) => isActive(d.run.status))
  // The selected job lives in the URL, so the runs list can link straight to one.
  const select = (jobId: number) => location.replace(`#/runs/${id}/${jobId}`)
  const now = useNow()

  // Follow the first running (or failed) job until the user picks one.
  const jobs = data?.jobs ?? []
  const auto = jobs.find((j) => j.status === 'running') ?? jobs.find((j) => j.status === 'failed') ?? jobs[0]
  const current = jobs.find((j) => j.id === job) ?? auto

  if (error && !data) return <p className="text-sm text-destructive">{error}</p>
  if (!data) return <Muted>Loading…</Muted>
  const { run } = data
  const meta = [run.workflow, shortRef(run.git_ref), run.sha?.slice(0, 7), run.trigger, duration(run.started_at, run.finished_at, now) || 'waiting'].filter(Boolean)

  return (
    <>
      <a href="#/" className="mb-8 inline-flex items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground">
        <ArrowLeftIcon className="size-3.5" /> Runs
      </a>
      <div className="mb-10 flex flex-col gap-4 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold tracking-tight">
            {run.project_name}/{run.repo_name} <span className="font-normal text-muted-foreground">#{run.id}</span>
          </h1>
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1">
            <StatusLabel status={run.status} />
            {meta.map((m, i) => (
              <span key={i} className="text-sm text-muted-foreground">
                · {m}
              </span>
            ))}
          </div>
        </div>
        {isActive(run.status) && (
          <Button variant="outline" onClick={() => attempt(() => api.cancel(run.id), 'Cancelling')}>
            Cancel
          </Button>
        )}
      </div>

      {run.error && <pre className="mb-8 overflow-x-auto rounded-md border border-destructive/40 p-4 font-mono text-xs whitespace-pre-wrap text-destructive">{run.error}</pre>}
      {jobs.length > 1 && (
        <div className="mb-6">
          <JobBar jobs={jobs} />
          <p className="mt-2 text-xs text-muted-foreground">{jobCounts(jobs)}</p>
        </div>
      )}
      {jobs.length > 0 && <JobGraph jobs={jobs} selected={current?.id} onSelect={select} now={now} />}
      {current && current.status === 'queued' && <Muted>{current.name} is waiting to start.</Muted>}
      {current && !['queued', 'skipped'].includes(current.status) && <LogView key={current.id} job={current} />}
    </>
  )
}

/** Lay jobs out in columns by dependency depth. A need names a job, or all matrix instances `name (…)` of it. */
function JobGraph({ jobs, selected, onSelect, now }: { jobs: Job[]; selected?: number; onSelect: (id: number) => void; now: number }) {
  const columns = useMemo(() => {
    const base = (name: string) => name.replace(/ \(.*\)$/, '')
    const byBase = new Map<string, Job[]>()
    for (const j of jobs) {
      const k = base(j.name)
      byBase.set(k, [...(byBase.get(k) ?? []), j])
    }
    const depth = new Map<string, number>()
    const visit = (j: Job): number => {
      if (depth.has(j.name)) return depth.get(j.name)!
      depth.set(j.name, 0) // guards against cycles
      const d = Math.max(-1, ...j.needs.flatMap((n) => (byBase.get(n) ?? []).map(visit))) + 1
      depth.set(j.name, d)
      return d
    }
    jobs.forEach(visit)
    const cols: Job[][] = []
    for (const j of jobs) (cols[depth.get(j.name)!] ??= []).push(j)
    return cols
  }, [jobs])

  return (
    <div className="mb-8 overflow-x-auto">
      <div className="flex items-center gap-2">
        {columns.map((col, i) => (
          <div key={i} className="contents">
            {i > 0 && <div className="h-px w-6 shrink-0 bg-border" />}
            <div className="flex shrink-0 flex-col gap-1.5">
              {col.map((j) => (
                <button
                  key={j.id}
                  onClick={() => onSelect(j.id)}
                  className={cn(
                    'flex w-56 items-center gap-2.5 rounded-md border px-3 py-2 text-left text-sm transition-colors hover:border-foreground/40',
                    (j.status === 'skipped' || j.status === 'cancelled') && 'text-muted-foreground',
                    j.id === selected && 'border-foreground',
                  )}
                >
                  <StatusDot status={j.status} />
                  <span className="min-w-0 flex-1">
                    <span className="block truncate">{j.name}</span>
                    {j.step && <span className="block truncate text-xs text-muted-foreground">{j.step}</span>}
                  </span>
                  <span className="font-mono text-xs text-muted-foreground tabular-nums">{duration(j.started_at, j.finished_at, now)}</span>
                </button>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
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
    // The server replays the whole log on every (re)connect, so start over each time.
    es.onopen = () => {
      buffer.current = []
      setLines([])
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
    <div className="overflow-hidden rounded-md border">
      <div className="flex items-center gap-3 border-b px-4 py-2.5 text-sm">
        <StatusDot status={job.status} />
        <span className="font-medium">{job.name}</span>
        {job.exit_code != null && job.exit_code !== 0 && <span className="text-muted-foreground">exit {job.exit_code}</span>}
        <span className="ml-auto font-mono text-xs text-muted-foreground tabular-nums">{lines.length} lines</span>
        {!follow && (
          <Button variant="ghost" size="xs" onClick={() => setFollow(true)}>
            <ArrowDownIcon /> Follow
          </Button>
        )}
      </div>
      <div ref={box} onScroll={onScroll} className="h-[62vh] overflow-auto bg-[#0a0a0a] py-3 font-mono text-[12.5px] leading-relaxed text-neutral-400">
        {lines.map((l, i) => (
          <div
            key={i}
            className={cn(
              'flex px-4 break-all whitespace-pre-wrap hover:bg-white/[0.03]',
              l.startsWith('▶') && 'mt-2 font-medium text-neutral-100',
              l.startsWith('✗') && 'text-[#ff6166]',
              (l.startsWith('●') || l.startsWith('◦')) && 'text-neutral-600',
            )}
          >
            <span className="w-12 shrink-0 pr-4 text-right text-neutral-700 select-none">{i + 1}</span>
            <span>{l}</span>
          </div>
        ))}
      </div>
    </div>
  )
}
