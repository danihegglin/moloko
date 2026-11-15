import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ArrowDownIcon, ChevronRightIcon, TriangleAlertIcon } from 'lucide-react'
import { api, duration, groupSpan, groupStatus, isActive, jobCounts, type Job, type RunWithJobs } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { cn } from '@/lib/utils'
import { attempt, Card, CopyButton, ErrorNote, JobBar, ListSkeleton, RefChip, Sha, StatusBadge, StatusIcon, Trigger } from '@/components/common'
import { Button } from '@/components/ui/button'

export function RunPage({ id, job }: { id: number; job?: number }) {
  const { data, error } = usePoll(() => api.run(id), 1000, [id], (d) => d.group.some((r) => isActive(r.status)))
  // The selected job lives in the URL, so the runs list can link straight to one.
  const select = (jobId: number) => location.replace(`#/runs/${id}/${jobId}`)
  const now = useNow()

  // Follow the first running (or failed) job until the user picks one.
  const jobs = data?.jobs ?? []
  const auto = jobs.find((j) => j.status === 'running') ?? jobs.find((j) => j.status === 'failed') ?? jobs[0]
  const current = jobs.find((j) => j.id === job) ?? auto

  if (error && !data) return <ErrorNote>{error}</ErrorNote>
  if (!data) return <ListSkeleton rows={3} />
  const { run, group } = data
  // One trigger can start several pipelines. The header is about all of them; the rest about the one selected.
  const many = group.length > 1
  const status = groupStatus(group)
  const [start, end] = groupSpan(group)
  const active = group.filter((r) => isActive(r.status))
  const pipeline = run.workflow ?? run.repo_name

  return (
    <>
      <nav className="mb-5 flex items-center gap-1.5 text-sm text-muted-foreground">
        <a href="#/" className="transition-colors hover:text-foreground">
          Runs
        </a>
        <ChevronRightIcon className="size-3.5 opacity-60" />
        <span>
          {run.project_name}/{run.repo_name}
        </span>
        <ChevronRightIcon className="size-3.5 opacity-60" />
        <span className="font-mono text-foreground">#{run.group_id}</span>
      </nav>
      <div className="mb-6 flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
          <h1 className="truncate text-2xl font-semibold tracking-tight">{many ? `${run.project_name}/${run.repo_name}` : pipeline}</h1>
          <span className="font-mono text-xl text-muted-foreground">#{run.group_id}</span>
          <StatusBadge status={status} />
        </div>
        {active.length > 0 && (
          <Button variant="outline" className="self-start bg-card shadow-xs sm:self-auto" onClick={() => attempt(() => Promise.all(active.map((r) => api.cancel(r.id))), 'Cancelling')}>
            {active.length > 1 ? 'Cancel all' : 'Cancel run'}
          </Button>
        )}
      </div>

      <Card className="mb-8">
        <dl className="grid grid-cols-2 gap-px bg-border sm:grid-cols-3 lg:grid-cols-5">
          <Fact label="Repository">
            <span className="truncate">
              {run.project_name}/{run.repo_name}
            </span>
          </Fact>
          <Fact label="Ref">
            <RefChip gitRef={run.git_ref} />
          </Fact>
          <Fact label="Commit">{run.sha ? <Sha sha={run.sha} className="text-sm text-foreground" /> : <span className="text-muted-foreground">Not resolved</span>}</Fact>
          <Fact label="Started">
            <span className="inline-flex items-center gap-2">
              <span title={new Date(run.created_at).toLocaleString()}>{new Date(run.created_at).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })}</span>
              <Trigger trigger={run.trigger} className="text-xs text-muted-foreground" />
            </span>
          </Fact>
          <Fact label="Duration" className="col-span-2 sm:col-span-2 lg:col-span-1">
            <span className="tabular-nums">{duration(start, end, now) || 'Waiting'}</span>
          </Fact>
        </dl>
      </Card>

      {many && <Pipelines group={group} selected={run.id} now={now} />}

      {run.error && (
        <div className="mb-8 flex gap-3 rounded-xl border border-destructive/30 bg-destructive/5 p-4 text-destructive">
          <TriangleAlertIcon className="mt-0.5 size-4 shrink-0" />
          <div className="min-w-0">
            <p className="text-sm font-medium">{many ? `${pipeline} did not start` : 'The run did not start'}</p>
            <pre className="mt-1 overflow-x-auto font-mono text-xs whitespace-pre-wrap">{run.error}</pre>
          </div>
        </div>
      )}
      {jobs.length > 0 && (
        <section className="mb-8">
          <SectionTitle title={many ? `Jobs in ${pipeline}` : 'Jobs'} aside={jobCounts(jobs)} />
          <Card>
            {jobs.length > 1 && <JobBar jobs={jobs} className="h-1 gap-0 [&>span]:rounded-none" />}
            <div className="thin-scroll overflow-x-auto p-4">
              <JobGraph jobs={jobs} selected={current?.id} onSelect={select} now={now} />
            </div>
          </Card>
        </section>
      )}
      {current && current.status === 'queued' && (
        <Notice status="queued" title={`${current.name} is waiting to start`}>
          {current.needs.length > 0 ? `It runs after ${current.needs.join(', ')}.` : 'It starts as soon as an executor is free.'}
        </Notice>
      )}
      {current && current.status === 'skipped' && (
        <Notice status="skipped" title={`${current.name} was skipped`}>
          Its condition was not met, or a job it needs did not pass.
        </Notice>
      )}
      {current && !['queued', 'skipped'].includes(current.status) && <LogView key={current.id} job={current} now={now} />}
    </>
  )
}

function Fact({ label, children, className }: { label: string; children: ReactNode; className?: string }) {
  return (
    <div className={cn('min-w-0 bg-card px-4 py-3', className)}>
      <dt className="text-xs font-medium text-muted-foreground">{label}</dt>
      <dd className="mt-1.5 flex h-6 min-w-0 items-center text-sm">{children}</dd>
    </div>
  )
}

function SectionTitle({ title, aside }: { title: string; aside?: ReactNode }) {
  return (
    <div className="mb-3 flex items-baseline justify-between gap-4">
      <h2 className="text-sm font-semibold">{title}</h2>
      {aside && <span className="text-xs text-muted-foreground">{aside}</span>}
    </div>
  )
}

function Notice({ status, title, children }: { status: Job['status']; title: string; children: ReactNode }) {
  return (
    <Card className="flex items-start gap-3 p-4">
      <StatusIcon status={status} className="mt-0.5" />
      <div>
        <p className="text-sm font-medium">{title}</p>
        <p className="mt-0.5 text-sm text-muted-foreground">{children}</p>
      </div>
    </Card>
  )
}

/** The pipelines one trigger started; picking one shows its jobs and logs below. */
function Pipelines({ group, selected, now }: { group: RunWithJobs[]; selected: number; now: number }) {
  return (
    <section className="mb-8">
      <SectionTitle title="Pipelines" aside={`${group.length} started by this ${group[0].trigger === 'manual' ? 'run' : group[0].trigger}`} />
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {group.map((r) => (
          <a
            key={r.id}
            href={`#/runs/${r.id}`}
            aria-current={r.id === selected ? 'true' : undefined}
            className={cn(
              'rounded-xl border bg-card p-3.5 shadow-xs transition-colors hover:border-foreground/30',
              r.id === selected && 'border-foreground/70 ring-1 ring-foreground/70 hover:border-foreground/70',
            )}
          >
            <div className="flex items-center gap-2.5 text-sm">
              <StatusIcon status={r.status} />
              <span className="min-w-0 flex-1 truncate font-medium">{r.workflow ?? r.repo_name}</span>
              <span className="text-xs text-muted-foreground tabular-nums">{duration(r.started_at, r.finished_at, now)}</span>
            </div>
            <JobBar jobs={r.jobs} className="mt-3 h-1" />
            <div className="mt-2 truncate text-xs text-muted-foreground">{jobCounts(r.jobs) || (r.error ? 'Did not start' : 'Waiting')}</div>
          </a>
        ))}
      </div>
    </section>
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
    <div className="flex items-center">
      {columns.map((col, i) => (
        <div key={i} className="contents">
          {i > 0 && (
            <div className="flex shrink-0 items-center text-muted-foreground/50">
              <div className="h-px w-5 bg-current" />
              <ChevronRightIcon className="-ml-2 size-4" />
            </div>
          )}
          <div className="flex shrink-0 flex-col gap-2">
            {col.map((j) => (
              <button
                key={j.id}
                onClick={() => onSelect(j.id)}
                aria-pressed={j.id === selected}
                className={cn(
                  'flex h-[3.25rem] w-60 items-center gap-2.5 rounded-lg border bg-background px-3 text-left text-sm transition-colors hover:border-foreground/30',
                  (j.status === 'skipped' || j.status === 'cancelled') && 'text-muted-foreground',
                  j.id === selected && 'border-foreground/70 bg-card ring-1 ring-foreground/70 hover:border-foreground/70',
                )}
              >
                <StatusIcon status={j.status} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate font-medium">{j.name}</span>
                  {j.step && <span className="block truncate text-xs text-muted-foreground">{j.step.replace(/^▶ /, '')}</span>}
                </span>
                <span className="text-xs text-muted-foreground tabular-nums">{duration(j.started_at, j.finished_at, now)}</span>
              </button>
            ))}
          </div>
        </div>
      ))}
    </div>
  )
}

const ANSI = /\x1b\[[0-9;]*[A-Za-z]/g

/** A `▶` line starts a step; the lines before the first one belong to no step. */
interface Step {
  title: string | null
  /** Index of the step's first line in the whole log. */
  at: number
  lines: string[]
  failed: boolean
}

function steps(lines: string[]): Step[] {
  const out: Step[] = [{ title: null, at: 0, lines: [], failed: false }]
  lines.forEach((l, i) => {
    if (l.startsWith('▶')) out.push({ title: l.replace(/^▶ ?/, ''), at: i, lines: [], failed: false })
    else {
      const s = out[out.length - 1]
      s.lines.push(l)
      if (l.startsWith('✗')) s.failed = true
    }
  })
  return out[0].lines.length ? out : out.slice(1)
}

function LogView({ job, now }: { job: Job; now: number }) {
  const [lines, setLines] = useState<string[]>([])
  const [follow, setFollow] = useState(true)
  const [closed, setClosed] = useState<Set<number>>(new Set())
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

  const sections = useMemo(() => steps(lines), [lines])
  const toggle = (at: number) =>
    setClosed((prev) => {
      const next = new Set(prev)
      if (!next.delete(at)) next.add(at)
      return next
    })

  return (
    <Card>
      <div className="flex h-12 items-center gap-3 border-b px-4 text-sm">
        <StatusIcon status={job.status} />
        <span className="truncate font-medium">{job.name}</span>
        {job.exit_code != null && job.exit_code !== 0 && <span className="rounded-md border border-destructive/25 bg-destructive/10 px-1.5 py-0.5 font-mono text-xs text-destructive">exit {job.exit_code}</span>}
        <span className="ml-auto hidden shrink-0 text-xs text-muted-foreground tabular-nums sm:inline">
          {[duration(job.started_at, job.finished_at, now), `${lines.length} lines`].filter(Boolean).join(' · ')}
        </span>
        <div className="ml-auto flex items-center gap-1 sm:ml-0">
          {!follow && (
            <Button variant="outline" size="xs" onClick={() => setFollow(true)}>
              <ArrowDownIcon /> Jump to end
            </Button>
          )}
          <CopyButton value={() => lines.join('\n')} label="Copied the log" />
        </div>
      </div>
      <div ref={box} onScroll={onScroll} className="thin-scroll h-[65vh] min-h-80 overflow-auto bg-[#0a0a0a] pb-3 font-mono text-[12.5px] leading-relaxed text-neutral-400">
        {lines.length === 0 && <p className="px-4 pt-4 text-neutral-600">{isActive(job.status) ? 'Waiting for output…' : 'No output.'}</p>}
        {sections.map((s, n) => {
          const open = !closed.has(s.at) && s.lines.length > 0
          const last = n === sections.length - 1
          return (
            <div key={s.at}>
              {s.title != null && (
                <button
                  onClick={() => toggle(s.at)}
                  aria-expanded={open}
                  disabled={s.lines.length === 0}
                  className="sticky top-0 z-10 flex w-full items-center gap-2 border-b border-white/[0.06] bg-[#111] px-4 py-1.5 text-left text-neutral-100 enabled:hover:bg-[#161616]"
                >
                  <ChevronRightIcon className={cn('size-3.5 shrink-0 text-neutral-500 transition-transform', open && 'rotate-90', s.lines.length === 0 && 'invisible')} />
                  <span
                    className={cn(
                      'size-1.5 shrink-0 rounded-full',
                      s.failed ? 'bg-[#ff6166]' : last && job.status === 'running' ? 'animate-pulse bg-[#60a5fa]' : last && job.status !== 'success' ? 'bg-neutral-500' : 'bg-[#22c55e]',
                    )}
                  />
                  <span className="min-w-0 flex-1 truncate font-medium">{s.title}</span>
                  <span className="shrink-0 text-[11px] text-neutral-500 tabular-nums">
                    {s.lines.length > 0 && `${s.lines.length} ${s.lines.length === 1 ? 'line' : 'lines'}`}
                  </span>
                </button>
              )}
              {open && (
                <div className="py-1.5">
                  {s.lines.map((l, i) => (
                    <div
                      key={i}
                      className={cn(
                        'flex px-4 break-all whitespace-pre-wrap hover:bg-white/[0.04]',
                        l.startsWith('✗') && 'bg-[#ff6166]/10 text-[#ff6166]',
                        /^(error|Error|ERROR)\b/.test(l) && 'text-[#ff8a8e]',
                        /^(warning|Warning|WARN)\b/.test(l) && 'text-[#e5b567]',
                        (l.startsWith('●') || l.startsWith('◦')) && 'text-neutral-600',
                      )}
                    >
                      <span className="w-12 shrink-0 pr-4 text-right text-neutral-700 select-none">{s.at + i + (s.title == null ? 1 : 2)}</span>
                      <span>{l}</span>
                    </div>
                  ))}
                </div>
              )}
            </div>
          )
        })}
      </div>
    </Card>
  )
}
