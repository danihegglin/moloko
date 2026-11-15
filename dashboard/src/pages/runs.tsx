import { useState } from 'react'
import { api, ago, duration, isActive, shortRef, type Job, type Run } from '@/api'
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
          {runs.data.map((r) => (
            <li key={r.id}>
              <a href={`#/runs/${r.id}`} className="-mx-3 flex items-center gap-4 rounded-md px-3 py-3.5 transition-colors hover:bg-muted/60">
                <StatusDot status={r.status} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-baseline gap-2 truncate text-sm">
                    <span className="font-medium">{r.workflow ?? r.repo_name}</span>
                    <span className="truncate text-muted-foreground">
                      {r.project_name}/{r.repo_name}
                    </span>
                  </div>
                  <div className="mt-0.5 flex items-center gap-2 truncate font-mono text-xs text-muted-foreground">
                    <span>#{r.id}</span>
                    <span>{shortRef(r.git_ref)}</span>
                    {r.sha && <span>{r.sha.slice(0, 7)}</span>}
                    <span className="hidden font-sans sm:inline">· {r.trigger}</span>
                  </div>
                </div>
                {r.jobs.length > 1 && <JobBar jobs={r.jobs} className="hidden w-24 shrink-0 sm:flex" />}
                <div className="w-16 shrink-0 text-right text-xs text-muted-foreground tabular-nums">
                  <div>{ago(r.created_at, now)}</div>
                  <div className="mt-0.5">{duration(r.started_at, r.finished_at, now) || '—'}</div>
                </div>
              </a>
              {isActive(r.status) && r.jobs.length > 0 && <ActiveJobs runId={r.id} jobs={r.jobs} now={now} />}
            </li>
          ))}
        </ul>
      )}
    </>
  )
}

/** What a run in progress is doing right now: every job with the step it is on. */
function ActiveJobs({ runId, jobs, now }: { runId: number; jobs: Job[]; now: number }) {
  return (
    <ul className="mb-3 ml-[3px] border-l pl-5">
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

function Summary({ runs }: { runs: (Run & { jobs: Job[] })[] }) {
  const active = runs.filter((r) => isActive(r.status)).length
  const done = runs.filter((r) => r.status === 'success' || r.status === 'failed')
  const ok = done.filter((r) => r.status === 'success').length
  const jobs = runs.flatMap((r) => r.jobs).filter((j) => j.status === 'running').length
  const parts = [active ? `${active} ${active === 1 ? 'run' : 'runs'} in progress, ${jobs} ${jobs === 1 ? 'job' : 'jobs'} running` : null, done.length ? `${Math.round((ok / done.length) * 100)}% passing` : null, `${runs.length} recent`]
  return <>{parts.filter(Boolean).join(' · ')}</>
}
