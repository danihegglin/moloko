import { useState } from 'react'
import { ChevronRightIcon, InboxIcon, PlayIcon } from 'lucide-react'
import { api, ago, duration, groupFocus, groupSpan, groupStatus, isActive, type Job, type RunWithJobs, type Status } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { cn } from '@/lib/utils'
import { Card, EmptyState, ErrorNote, JobBar, ListSkeleton, Pill, RefChip, Sha, STATUS_LABEL, StatusIcon, Trigger } from '@/components/common'
import { TriggerDialog } from '@/components/trigger'
import { Button } from '@/components/ui/button'
import { Select, SelectContent, SelectGroup, SelectItem, SelectLabel, SelectSeparator, SelectTrigger, SelectValue } from '@/components/ui/select'

const ALL = 'all'

type Tab = 'all' | 'active' | 'failed' | 'success'
const TABS: [Tab, string][] = [['all', 'All'], ['active', 'In progress'], ['failed', 'Failed'], ['success', 'Passed']]
const inTab = (tab: Tab, s: Status) => tab === 'all' || (tab === 'active' ? isActive(s) : s === tab)

export function RunsPage() {
  const [filter, setFilter] = useState(ALL)
  const [tab, setTab] = useState<Tab>('all')
  const [running, setRunning] = useState(false)
  const projects = usePoll(api.projects, 30000, [])
  const runs = usePoll(() => api.runs(filter === ALL ? undefined : filter), 2000, [filter])
  const now = useNow()
  const repos = projects.data?.flatMap((p) => p.repos) ?? []
  const filterRepo = filter.startsWith('r') ? Number(filter.slice(1)) : undefined
  const filterProject = filter.startsWith('p') ? Number(filter.slice(1)) : undefined
  const defaultRepo = filterRepo ?? repos.find((r) => r.project_id === filterProject)?.id ?? repos[0]?.id
  const groups = runs.data ? grouped(runs.data) : []
  const shown = groups.filter((g) => inTab(tab, groupStatus(g)))

  return (
    <>
      <TriggerDialog open={running} onOpenChange={setRunning} projects={projects.data ?? []} defaultRepo={defaultRepo} />

      {runs.error && <ErrorNote>{runs.error}</ErrorNote>}

      <div className="mb-5 flex flex-wrap items-center gap-x-4 gap-y-3">
        <h1 className="mr-2 text-2xl font-semibold tracking-tight">Runs</h1>
        <div className="inline-flex rounded-lg border bg-card p-0.5 shadow-xs">
          {TABS.map(([t, label]) => {
            const n = groups.filter((g) => inTab(t, groupStatus(g))).length
            return (
              <button
                key={t}
                onClick={() => setTab(t)}
                aria-pressed={tab === t}
                className={cn('flex h-7 items-center gap-1.5 rounded-md px-2.5 text-[13px] font-medium text-muted-foreground transition-colors hover:text-foreground', tab === t && 'bg-muted text-foreground')}
              >
                {label}
                <span className="text-xs font-normal text-muted-foreground tabular-nums">{n}</span>
              </button>
            )
          })}
        </div>
        <Select value={filter} onValueChange={setFilter}>
          <SelectTrigger className="ml-auto w-48 bg-card shadow-xs">
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
        {repos.length > 0 && (
          <Button onClick={() => setRunning(true)}>
            <PlayIcon className="fill-current" /> New run
          </Button>
        )}
      </div>

      {!runs.data ? (
        <ListSkeleton rows={6} />
      ) : groups.length === 0 ? (
        <EmptyState icon={InboxIcon} title="No runs yet">
          {repos.length === 0 ? (
            <>
              Add a repository under{' '}
              <a href="/projects" className="text-foreground underline underline-offset-4">
                Projects
              </a>
              .
            </>
          ) : (
            <>
              Commit a <code className="font-mono">.moloko.yml</code> or GitHub workflows, then start a run, turn on <span className="text-foreground">Build on push</span> for the repository, or point a GitHub
              webhook at <code className="font-mono">/api/webhooks/github</code>.
            </>
          )}
        </EmptyState>
      ) : shown.length === 0 ? (
        <EmptyState icon={InboxIcon} title={`No ${tab === 'active' ? 'runs in progress' : tab === 'failed' ? 'failed runs' : 'passed runs'}`}>
          Nothing among the recent runs matches this filter.
        </EmptyState>
      ) : (
        <Card>
          <div className="hidden items-center gap-4 border-b bg-muted/40 px-4 py-2 text-xs font-medium text-muted-foreground md:flex">
            <span className="w-4" />
            <span className="flex-1">Run</span>
            <span className="w-60">Ref</span>
            <span className="w-28">Jobs</span>
            <span className="w-20 text-right">Duration</span>
            <span className="w-4" />
          </div>
          <ul className="divide-y">
            {shown.map((g) => (
              <GroupRow key={g[0].group_id} runs={g} now={now} />
            ))}
          </ul>
        </Card>
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
  const done = jobs.filter((j) => !isActive(j.status)).length
  const [start, end] = groupSpan(runs)
  return (
    <li>
      <a href={`/runs/${groupFocus(runs).id}`} className="group flex items-center gap-4 px-4 py-3.5 transition-colors hover:bg-muted/50">
        <StatusIcon status={status} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 text-sm">
            <span className="truncate font-medium">{runs.map((r) => r.workflow ?? r.repo_name).join(', ')}</span>
            {runs.length > 1 && <Pill>{runs.length} pipelines</Pill>}
          </div>
          <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
            <span className="truncate">
              {first.project_name}/{first.repo_name}
            </span>
            <span className="text-muted-foreground/40">·</span>
            <span className="font-mono">#{first.group_id}</span>
            <span className="text-muted-foreground/40">·</span>
            <Trigger trigger={first.trigger} />
          </div>
        </div>
        <div className="hidden w-60 min-w-0 items-center gap-2.5 md:flex">
          <RefChip gitRef={first.git_ref} className="max-w-36" />
          {first.sha && <Sha sha={first.sha} />}
        </div>
        <div className="hidden w-28 sm:block">
          {jobs.length > 0 ? (
            <>
              <JobBar jobs={jobs} />
              <div className="mt-1.5 text-[11px] text-muted-foreground tabular-nums">{isActive(status) ? `${done} of ${jobs.length} done` : `${jobs.length} ${jobs.length === 1 ? 'job' : 'jobs'}`}</div>
            </>
          ) : (
            <span className="text-xs text-muted-foreground">{first.error ? 'Did not start' : '—'}</span>
          )}
        </div>
        <div className="w-20 shrink-0 text-right text-xs tabular-nums">
          <div className="font-medium">{duration(start, end, now) || '—'}</div>
          <div className="mt-1 text-muted-foreground">{ago(first.created_at, now)}</div>
        </div>
        <ChevronRightIcon className="size-4 shrink-0 text-muted-foreground/50 transition-transform group-hover:translate-x-0.5 group-hover:text-muted-foreground" />
      </a>
      {isActive(status) && jobs.length > 0 && (
        <div className="border-t border-dashed bg-muted/30 py-2 pr-4 pl-12">
          {runs.map((r) => (
            <div key={r.id}>
              {runs.length > 1 && (
                <a href={`/runs/${r.id}`} className="-mx-2 flex items-center gap-2.5 rounded-md px-2 py-1.5 text-[13px] font-medium transition-colors hover:bg-foreground/5">
                  <StatusIcon status={r.status} className="size-3.5" />
                  <span className="min-w-0 flex-1 truncate">{r.workflow ?? r.repo_name}</span>
                  <span className="shrink-0 text-xs font-normal text-muted-foreground tabular-nums">{duration(r.started_at, r.finished_at, now) || STATUS_LABEL[r.status].toLowerCase()}</span>
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
    <ul className={nested ? 'mb-1 ml-[7px] border-l pl-4' : undefined}>
      {jobs
        .filter((j) => j.status !== 'skipped')
        .map((j) => (
          <li key={j.id}>
            <a href={`/runs/${runId}/${j.id}`} className="-mx-2 flex items-center gap-2.5 rounded-md px-2 py-1.5 text-[13px] transition-colors hover:bg-foreground/5">
              <StatusIcon status={j.status} className="size-3.5" />
              <span className={cn('shrink-0', j.status !== 'running' && 'text-muted-foreground')}>{j.name}</span>
              <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{j.status === 'queued' ? 'Waiting' : j.step?.replace(/^▶ /, '')}</span>
              <span className="shrink-0 text-xs text-muted-foreground tabular-nums">{duration(j.started_at, j.finished_at, now)}</span>
            </a>
          </li>
        ))}
    </ul>
  )
}
