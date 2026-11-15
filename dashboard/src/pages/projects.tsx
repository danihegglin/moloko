import { useState } from 'react'
import { FolderGit2Icon, FolderIcon, MoreHorizontalIcon, PlayIcon, PlusIcon, RefreshCwIcon } from 'lucide-react'
import { api, ago, type Project, type Repo, type RunWithJobs } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { attempt, Card, ConfirmDialog, EmptyState, ErrorNote, ListSkeleton, PageHeader, Pill, StatusIcon } from '@/components/common'
import { TriggerDialog } from '@/components/trigger'
import { SecretsDialog } from '@/components/secrets'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

export function ProjectsPage() {
  const [version, setVersion] = useState(0)
  const projects = usePoll(api.projects, 30000, [version])
  const runs = usePoll(() => api.runs(), 5000, [])
  const now = useNow(10000)
  const refresh = () => setVersion((v) => v + 1)
  const [creating, setCreating] = useState(false)
  const [runRepo, setRunRepo] = useState<number | null>(null)
  // The list is newest first, so the first run seen for a repository is its latest.
  const latest = new Map<number, RunWithJobs>()
  for (const r of runs.data ?? []) if (!latest.has(r.repo_id)) latest.set(r.repo_id, r)

  return (
    <>
      <PageHeader title="Projects" description="A project groups repositories. Each repository runs its own .moloko.yml and GitHub workflows.">
        <Button onClick={() => setCreating(true)}>
          <PlusIcon /> New project
        </Button>
      </PageHeader>
      <ProjectDialog open={creating} onOpenChange={setCreating} onDone={refresh} />
      <TriggerDialog open={runRepo != null} onOpenChange={(o) => !o && setRunRepo(null)} projects={projects.data ?? []} defaultRepo={runRepo ?? undefined} />

      {projects.error && <ErrorNote>{projects.error}</ErrorNote>}
      {!projects.data ? (
        <ListSkeleton />
      ) : projects.data.length === 0 ? (
        <EmptyState icon={FolderIcon} title="No projects yet" action={<Button onClick={() => setCreating(true)}>New project</Button>}>
          Create a project, then add its repositories.
        </EmptyState>
      ) : (
        <div className="space-y-6">
          {projects.data.map((p) => (
            <ProjectSection key={p.id} project={p} latest={latest} now={now} onChange={refresh} onRun={setRunRepo} />
          ))}
        </div>
      )}
    </>
  )
}

function ProjectSection({ project, latest, now, onChange, onRun }: { project: Project; latest: Map<number, RunWithJobs>; now: number; onChange: () => void; onRun: (repo: number) => void }) {
  const [dialog, setDialog] = useState<'rename' | 'delete' | 'repo' | 'secrets' | null>(null)
  const close = (open: boolean) => !open && setDialog(null)
  const n = project.repos.length
  return (
    <Card>
      <div className="flex items-center gap-3 border-b bg-muted/40 py-2.5 pr-3 pl-4">
        <FolderIcon className="size-4 shrink-0 text-muted-foreground" />
        <h2 className="truncate text-sm font-semibold">{project.name}</h2>
        <Pill>
          {n} {n === 1 ? 'repository' : 'repositories'}
        </Pill>
        <div className="ml-auto flex items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => setDialog('repo')}>
            <PlusIcon /> Add repository
          </Button>
          <Menu>
            <DropdownMenuItem onSelect={() => setDialog('secrets')}>Secrets</DropdownMenuItem>
            <DropdownMenuItem onSelect={() => setDialog('rename')}>Rename</DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem variant="destructive" onSelect={() => setDialog('delete')}>
              Delete project
            </DropdownMenuItem>
          </Menu>
        </div>
      </div>
      {n === 0 ? (
        <p className="px-4 py-10 text-center text-sm text-muted-foreground">No repositories yet. Add one to start building.</p>
      ) : (
        <ul className="divide-y">
          {project.repos.map((r) => (
            <RepoRow key={r.id} repo={r} last={latest.get(r.id)} now={now} onChange={onChange} onRun={() => onRun(r.id)} />
          ))}
        </ul>
      )}

      <ProjectDialog open={dialog === 'rename'} onOpenChange={close} project={project} onDone={onChange} />
      <SecretsDialog open={dialog === 'secrets'} onOpenChange={close} scope={`projects/${project.id}`} label={project.name} kind="project" />
      <RepoDialog open={dialog === 'repo'} onOpenChange={close} projectId={project.id} onDone={onChange} />
      <ConfirmDialog
        open={dialog === 'delete'}
        onOpenChange={close}
        title={`Delete ${project.name}?`}
        description={n ? `This also deletes its ${n} ${n === 1 ? 'repository' : 'repositories'} and all of their runs.` : 'This project has no repositories.'}
        action="Delete project"
        onConfirm={() => attempt(() => api.deleteProject(project.id), `Deleted ${project.name}`).then(onChange)}
      />
    </Card>
  )
}

function Menu({ children }: { children: React.ReactNode }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon-sm" className="text-muted-foreground" aria-label="More">
          <MoreHorizontalIcon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="min-w-40">
        {children}
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function RepoRow({ repo, last, now, onChange, onRun }: { repo: Repo; last?: RunWithJobs; now: number; onChange: () => void; onRun: () => void }) {
  const [dialog, setDialog] = useState<'edit' | 'delete' | 'secrets' | null>(null)
  const close = (open: boolean) => !open && setDialog(null)
  return (
    <li className="flex items-center gap-3 py-3 pr-3 pl-4">
      <div className="grid size-9 shrink-0 place-items-center rounded-lg border bg-muted/50">
        <FolderGit2Icon className="size-4 text-muted-foreground" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-sm">
          <span className="truncate font-medium">{repo.name}</span>
          {repo.poll && (
            <Pill className="border-running/25 bg-running/10 text-running">
              <RefreshCwIcon className="size-2.5" /> Builds on push
            </Pill>
          )}
        </div>
        <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">{repo.url}</div>
      </div>
      {last ? (
        <a href={`/runs/${last.id}`} className="hidden items-center gap-2 rounded-md px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground md:flex" title="Latest run">
          <StatusIcon status={last.status} className="size-3.5" />
          <span className="max-w-32 truncate">{last.workflow ?? 'Run'}</span>
          <span className="font-mono">#{last.group_id}</span>
          <span>{ago(last.created_at, now)}</span>
        </a>
      ) : (
        <span className="hidden px-2 text-xs text-muted-foreground md:block">No recent runs</span>
      )}
      <Button variant="outline" size="sm" onClick={onRun}>
        <PlayIcon className="fill-current" /> Run
      </Button>
      <Menu>
        <DropdownMenuItem onSelect={() => setDialog('secrets')}>Secrets</DropdownMenuItem>
        <DropdownMenuItem onSelect={() => setDialog('edit')}>Edit</DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem variant="destructive" onSelect={() => setDialog('delete')}>
          Delete
        </DropdownMenuItem>
      </Menu>
      <RepoDialog open={dialog === 'edit'} onOpenChange={close} projectId={repo.project_id} repo={repo} onDone={onChange} />
      <SecretsDialog open={dialog === 'secrets'} onOpenChange={close} scope={`repos/${repo.id}`} label={`${repo.project_name}/${repo.name}`} kind="repo" />
      <ConfirmDialog
        open={dialog === 'delete'}
        onOpenChange={close}
        title={`Delete ${repo.project_name}/${repo.name}?`}
        description="Deletes its runs, logs and local git mirror. The remote repository isn't touched."
        action="Delete repository"
        onConfirm={() => attempt(() => api.deleteRepo(repo.id), `Deleted ${repo.name}`).then(onChange)}
      />
    </li>
  )
}

function ProjectDialog({ open, onOpenChange, project, onDone }: { open: boolean; onOpenChange: (open: boolean) => void; project?: Project; onDone: () => void }) {
  const [name, setName] = useState(project?.name ?? '')
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    const ok = await attempt(() => (project ? api.renameProject(project.id, name) : api.createProject(name)), project ? 'Renamed' : `Created ${name}`)
    if (ok) {
      onOpenChange(false)
      if (!project) setName('')
      onDone()
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>{project ? 'Rename project' : 'New project'}</DialogTitle>
            <DialogDescription>{project ? 'This changes the image path of every repository in it.' : 'Add repositories once it exists.'}</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="project-name">Name</Label>
            <Input id="project-name" value={name} onChange={(e) => setName(e.target.value)} autoFocus required />
          </div>
          <DialogFooter>
            <Button type="submit">{project ? 'Save' : 'Create'}</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function RepoDialog({ open, onOpenChange, projectId, repo, onDone }: { open: boolean; onOpenChange: (open: boolean) => void; projectId: number; repo?: Repo; onDone: () => void }) {
  const [url, setUrl] = useState(repo?.url ?? '')
  const [name, setName] = useState(repo?.name ?? '')
  const [poll, setPoll] = useState(repo?.poll ?? false)
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    const ok = await attempt(() => (repo ? api.updateRepo(repo.id, { name, url, poll }) : api.createRepo(projectId, url, name, poll)), repo ? 'Saved' : 'Repository added')
    if (ok) {
      onOpenChange(false)
      if (!repo) {
        setUrl('')
        setName('')
        setPoll(false)
      }
      onDone()
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>{repo ? 'Edit repository' : 'Add repository'}</DialogTitle>
            <DialogDescription>Use an SSH URL for private repositories.</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="repo-url">Git URL</Label>
            <Input id="repo-url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="git@github.com:acme/api.git" className="font-mono" autoFocus required />
            {repo && url.trim() !== repo.url && <p className="text-xs text-muted-foreground">The local mirror is re-cloned on the next run.</p>}
          </div>
          <div className="grid gap-2">
            <Label htmlFor="repo-name">Name</Label>
            <Input id="repo-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="From the URL" required={!!repo} />
          </div>
          <label htmlFor="repo-poll" className="flex items-start gap-3 rounded-lg border bg-muted/30 p-3 text-sm">
            <input id="repo-poll" type="checkbox" checked={poll} onChange={(e) => setPoll(e.target.checked)} className="mt-0.5 size-4 accent-foreground" />
            <span>
              <span className="font-medium">Build on push</span>
              <span className="block text-xs text-muted-foreground">
                Checks the repository for new commits every minute and runs the pipelines that listen for pushes. Starts with the next commit; nothing already there is built.
              </span>
            </span>
          </label>
          <DialogFooter>
            <Button type="submit">{repo ? 'Save' : 'Add'}</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
