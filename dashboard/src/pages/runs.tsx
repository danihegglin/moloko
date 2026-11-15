import { useState } from 'react'
import { api, ago, duration, groupFocus, groupSpan, groupStatus, isActive, shortRef, type Job, type RunWithJobs } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { EmptyState, JobBar, Muted, PageHeader, StatusDot } from '@/components/common'
import { TriggerDialog } from '@/components/trigger'
import { Button } from '@/components/ui/button'
import { Select, SelectContent, SelectGroup, SelectItem, SelectLabel, SelectSeparator, SelectTrigger, SelectValue } from '@/components/ui/select'

const ALL = 'all'

export function RunsPage() {
  const [filter, setFilter] = useState(ALL)
  const [running, setRunning] = useState(false)
  const projects = usePoll(api.projects, 30000, [])
  const runs = usePoll(() => api.runs(filter === ALL ? undefined : filter), 2000, [filter])
  const now = useNow()
  const repos = projects.data?.flatMap((p) => p.repos) ?? []
  const filterRepo = filter.startsWith('r') ? Number(filter.slice(1)) : undefined
  const filterProject = filter.startsWith('p') ? Number(filter.slice(1)) : undefined
  const defaultRepo = filterRepo ?? repos.find((r) => r.project_id === filterProject)?.id ?? repos[0]?.id

  return (
    <>
      <PageHeader title="Runs" description={runs.data && runs.data.length > 0 ? <Summary runs={runs.data} /> : undefined}>
        <Select value={filter} onValueChange={setFilter}>
          <SelectTrigger className="w-48">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All projects</SelectItem>
            {projects.data?.map((p) => (
              <SelectGroup key={p.id}>
                <SelectSeparator />
                <SelectLabel>{p.name}</SelectLabel>
                <SelectItem value={`p${p.id}`}>All of {p.name}</SelectItem>
                {p.repos.map((r) => (
                  <SelectItem key={r.id} value={`r${r.id}`}>
                    {r.name}
                  </SelectItem>
                ))}
              </SelectGroup>
            ))}
          </SelectContent>
        </Select>
        {repos.length > 0 && <Button onClick={() => setRunning(true)}>New run</Button>}
      </PageHeader>
      <TriggerDialog open={running} onOpenChange={setRunning} projects={projects.data ?? []} defaultRepo={defaultRepo} />

      {runs.error && <p className="mb-4 text-sm text-destructive">{runs.error}</p>}
      {!runs.data ? (
        <Muted>Loading…</Muted>
      ) : runs.data.length === 0 ? (
        <EmptyState title="No runs yet">
          {repos.length === 0 ? (
            <>
              Add a repository under{' '}
              <a href="#/projects" className="text-foreground underline underline-offset-4">
                Projects
              </a>
              .
            </>
          ) : (
            <>
              Commit a <code className="font-mono">.moloko.yml</code> or GitHub workflows, then start a run or point a GitHub webhook at{' '}
              <code className="font-mono">/api/webhooks/github</code>.
            </>
          )}
        </EmptyState>
      ) : (
        <ul className="divide-y border-y">
          {grouped(runs.data).map((g) => (
            <GroupRow key={g[0].group_id} runs={g} now={now} />
          ))}
        </ul>
      )}
    </>
  )
}

/** Runs of the same trigger together, newest trigger first, each group oldest run first. */
function grouped(runs: RunWithJobs[]): RunWithJobs[][] {
  const groups = new Map<number, RunWithJobs[]>()
  for (const r of runs) groups.set(r.group_id, [r, ...(groups.get(r.group_id) ?? [])])
  return [...groups.values()]
}

/** One trigger: a single row, however many pipelines it started. */
function GroupRow({ runs, now }: { runs: RunWithJobs[]; now: number }) {
  const first = runs[0]
  const status = groupStatus(runs)
  const jobs = runs.flatMap((r) => r.jobs)
  const [start, end] = groupSpan(runs)
  return (
    <li>
      <a href={`#/runs/${groupFocus(runs).id}`} className="-mx-3 flex items-center gap-4 rounded-md px-3 py-3.5 transition-colors hover:bg-muted/60">
        <StatusDot status={status} />
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-2 text-sm">
            <span className="truncate font-medium">{runs.map((r) => r.workflow ?? r.repo_name).join(', ')}</span>
            <span className="shrink-0 text-muted-foreground">
              {first.project_name}/{first.repo_name}
            </span>
          </div>
          <div className="mt-0.5 flex items-center gap-2 truncate font-mono text-xs text-muted-foreground">
            <span>#{first.group_id}</span>
            <span>{shortRef(first.git_ref)}</span>
            {first.sha && <span>{first.sha.slice(0, 7)}</span>}
            <span className="hidden font-sans sm:inline">· {first.trigger}</span>
          </div>
        </div>
        {jobs.length > 1 && <JobBar jobs={jobs} className="hidden w-24 shrink-0 sm:flex" />}
        <div className="w-16 shrink-0 text-right text-xs text-muted-foreground tabular-nums">
          <div>{ago(first.created_at, now)}</div>
          <div className="mt-0.5">{duration(start, end, now) || '—'}</div>
        </div>
      </a>
      {isActive(status) && jobs.length > 0 && (
        <div className="mb-3 ml-[3px] border-l pl-5">
          {runs.map((r) => (
            <div key={r.id}>
              {runs.length > 1 && (
                <a href={`#/runs/${r.id}`} className="-mx-2 flex items-center gap-3 rounded-md px-2 py-1 text-sm font-medium transition-colors hover:bg-muted/60">
                  <StatusDot status={r.status} />
                  <span className="min-w-0 flex-1 truncate">{r.workflow ?? r.repo_name}</span>
                  <span className="shrink-0 font-mono text-xs font-normal text-muted-foreground tabular-nums">{duration(r.started_at, r.finished_at, now)}</span>
                </a>
              )}
              {isActive(r.status) && <ActiveJobs runId={r.id} jobs={r.jobs} now={now} nested={runs.length > 1} />}
            </div>
          ))}
        </div>
      )}
    </li>
  )
}

/** What a run in progress is doing right now: every job with the step it is on. */
function ActiveJobs({ runId, jobs, now, nested }: { runId: number; jobs: Job[]; now: number; nested: boolean }) {
  return (
    <ul className={nested ? 'mb-1 pl-5' : undefined}>
      {jobs
        .filter((j) => j.status !== 'skipped')
        .map((j) => (
          <li key={j.id}>
            <a href={`#/runs/${runId}/${j.id}`} className="-mx-2 flex items-center gap-3 rounded-md px-2 py-1 text-sm transition-colors hover:bg-muted/60">
              <StatusDot status={j.status} />
              <span className={j.status === 'running' ? 'shrink-0' : 'shrink-0 text-muted-foreground'}>{j.name}</span>
              <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{j.status === 'queued' ? 'waiting' : j.step}</span>
              <span className="shrink-0 font-mono text-xs text-muted-foreground tabular-nums">{duration(j.started_at, j.finished_at, now)}</span>
            </a>
          </li>
        ))}
    </ul>
  )
}

function Summary({ runs }: { runs: RunWithJobs[] }) {
  const statuses = grouped(runs).map(groupStatus)
  const active = statuses.filter(isActive).length
  const done = statuses.filter((s) => s === 'success' || s === 'failed')
  const ok = done.filter((s) => s === 'success').length
  const jobs = runs.flatMap((r) => r.jobs).filter((j) => j.status === 'running').length
  const parts = [
    active ? `${active} ${active === 1 ? 'run' : 'runs'} in progress, ${jobs} ${jobs === 1 ? 'job' : 'jobs'} running` : null,
    done.length ? `${Math.round((ok / done.length) * 100)}% passing` : null,
    `${statuses.length} recent`,
  ]
  return <>{parts.filter(Boolean).join(' · ')}</>
}
