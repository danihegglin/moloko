import { useState } from 'react'
import { MoreHorizontalIcon } from 'lucide-react'
import { api, type Project, type Repo } from '@/api'
import { usePoll } from '@/hooks'
import { attempt, ConfirmDialog, EmptyState, Muted, PageHeader } from '@/components/common'
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
  const refresh = () => setVersion((v) => v + 1)
  const [creating, setCreating] = useState(false)
  const [runRepo, setRunRepo] = useState<number | null>(null)

  return (
    <>
      <PageHeader title="Projects" description="A project groups repositories. Each repository runs its own .moloko.yml and GitHub workflows.">
        <Button onClick={() => setCreating(true)}>New project</Button>
      </PageHeader>
      <ProjectDialog open={creating} onOpenChange={setCreating} onDone={refresh} />
      <TriggerDialog open={runRepo != null} onOpenChange={(o) => !o && setRunRepo(null)} projects={projects.data ?? []} defaultRepo={runRepo ?? undefined} />

      {projects.error && <p className="mb-4 text-sm text-destructive">{projects.error}</p>}
      {!projects.data ? (
        <Muted>Loading…</Muted>
      ) : projects.data.length === 0 ? (
        <EmptyState title="No projects yet">Create a project, then add its repositories.</EmptyState>
      ) : (
        <div className="space-y-12">
          {projects.data.map((p) => (
            <ProjectSection key={p.id} project={p} onChange={refresh} onRun={setRunRepo} />
          ))}
        </div>
      )}
    </>
  )
}

function ProjectSection({ project, onChange, onRun }: { project: Project; onChange: () => void; onRun: (repo: number) => void }) {
  const [dialog, setDialog] = useState<'rename' | 'delete' | 'repo' | 'secrets' | null>(null)
  const close = (open: boolean) => !open && setDialog(null)
  const n = project.repos.length
  return (
    <section>
      <div className="mb-3 flex items-center gap-3">
        <h2 className="font-semibold tracking-tight">{project.name}</h2>
        <span className="text-sm text-muted-foreground">
          {n} {n === 1 ? 'repository' : 'repositories'}
        </span>
        <div className="ml-auto flex items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => setDialog('repo')}>
            Add repository
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
        <p className="rounded-md border border-dashed px-4 py-8 text-center text-sm text-muted-foreground">No repositories yet.</p>
      ) : (
        <ul className="divide-y border-y">
          {project.repos.map((r) => (
            <RepoRow key={r.id} repo={r} onChange={onChange} onRun={() => onRun(r.id)} />
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
    </section>
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

function RepoRow({ repo, onChange, onRun }: { repo: Repo; onChange: () => void; onRun: () => void }) {
  const [dialog, setDialog] = useState<'edit' | 'delete' | 'secrets' | null>(null)
  const close = (open: boolean) => !open && setDialog(null)
  return (
    <li className="flex items-center gap-4 py-3">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">{repo.name}</div>
        <div className="truncate font-mono text-xs text-muted-foreground">{repo.url}</div>
      </div>
      <Button variant="outline" size="sm" onClick={onRun}>
        Run
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
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    const ok = await attempt(() => (repo ? api.updateRepo(repo.id, { name, url }) : api.createRepo(projectId, url, name)), repo ? 'Saved' : 'Repository added')
    if (ok) {
      onOpenChange(false)
      if (!repo) {
        setUrl('')
        setName('')
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
          <DialogFooter>
            <Button type="submit">{repo ? 'Save' : 'Add'}</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
