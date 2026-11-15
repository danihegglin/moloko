import { useEffect, useState } from 'react'
import { api, type Pipeline, type PipelineInput, type Project } from '@/api'
import { attempt } from '@/components/common'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select, SelectContent, SelectGroup, SelectItem, SelectLabel, SelectTrigger, SelectValue } from '@/components/ui/select'

const ALL = '__all'
type Values = Record<string, string | boolean>

function defaults(p?: Pipeline): Values {
  const v: Values = {}
  for (const [k, spec] of Object.entries(p?.inputs ?? {})) {
    v[k] = spec.type === 'boolean' ? spec.default === true || spec.default === 'true' : String(spec.default ?? '')
  }
  return v
}

const needsInput = (p: Pipeline) => Object.values(p.inputs).some((i) => i.required && (i.default === undefined || i.default === ''))

/** Start a manual run: pick the repo, ref and pipeline, and fill in workflow_dispatch inputs. */
export function TriggerDialog({
  open,
  onOpenChange,
  projects,
  defaultRepo,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  projects: Project[]
  defaultRepo?: number
}) {
  const [repo, setRepo] = useState(defaultRepo ? String(defaultRepo) : '')
  const [ref, setRef] = useState('')
  const [pipelines, setPipelines] = useState<Pipeline[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [workflow, setWorkflow] = useState(ALL)
  const [values, setValues] = useState<Values>({})
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (open && defaultRepo) setRepo(String(defaultRepo))
  }, [open, defaultRepo])

  // Load the runnable pipelines whenever the repo or ref changes (debounced while typing).
  useEffect(() => {
    if (!open || !repo) return
    let stop = false
    setPipelines(null)
    setLoadError(null)
    const t = setTimeout(async () => {
      try {
        const list = await api.pipelines(Number(repo), ref)
        if (stop) return
        setPipelines(list)
        const pick = list.length === 1 ? list[0] : list.find(needsInput)
        setWorkflow(pick ? pick.file : ALL)
        setValues(defaults(pick))
      } catch (e) {
        if (!stop) setLoadError((e as Error).message)
      }
    }, 350)
    return () => {
      stop = true
      clearTimeout(t)
    }
  }, [open, repo, ref])

  const selected = pipelines?.find((p) => p.file === workflow)
  const blocked = workflow === ALL && pipelines?.filter(needsInput) || []

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    let id = 0
    const ok = await attempt(async () => {
      ;({ id } = await api.trigger(Number(repo), ref, workflow === ALL ? undefined : workflow, selected ? values : undefined))
    })
    setBusy(false)
    if (ok) {
      onOpenChange(false)
      location.hash = `#/runs/${id}`
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>New run</DialogTitle>
            <DialogDescription>Run a pipeline at a branch, tag or commit.</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label>Repository</Label>
            <Select value={repo} onValueChange={setRepo}>
              <SelectTrigger className="w-full">
                <SelectValue placeholder="Select a repository" />
              </SelectTrigger>
              <SelectContent>
                {projects
                  .filter((p) => p.repos.length > 0)
                  .map((p) => (
                    <SelectGroup key={p.id}>
                      <SelectLabel>{p.name}</SelectLabel>
                      {p.repos.map((r) => (
                        <SelectItem key={r.id} value={String(r.id)}>
                          {r.name}
                        </SelectItem>
                      ))}
                    </SelectGroup>
                  ))}
              </SelectContent>
            </Select>
          </div>
          <div className="grid gap-2">
            <Label htmlFor="ref">Ref</Label>
            <Input id="ref" placeholder="HEAD" value={ref} onChange={(e) => setRef(e.target.value)} className="font-mono" />
          </div>
          {repo && (
            <div className="grid gap-2">
              <Label>Pipeline</Label>
              {loadError ? (
                <p className="text-sm text-destructive">{loadError}</p>
              ) : !pipelines ? (
                <p className="text-sm text-muted-foreground">Loading…</p>
              ) : pipelines.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  Nothing runs manually here. Add a <code className="font-mono">.moloko.yml</code>, or <code className="font-mono">workflow_dispatch:</code> to a workflow.
                </p>
              ) : (
                <Select
                  value={workflow}
                  onValueChange={(w) => {
                    setWorkflow(w)
                    setValues(defaults(pipelines.find((p) => p.file === w)))
                  }}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {pipelines.length > 1 && <SelectItem value={ALL}>All pipelines</SelectItem>}
                    {pipelines.map((p) => (
                      <SelectItem key={p.file} value={p.file}>
                        {p.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              )}
              {blocked.length > 0 && (
                <p className="text-xs text-muted-foreground">
                  {blocked.map((p) => p.name).join(', ')} need{blocked.length === 1 ? 's' : ''} inputs. Select it to fill them in.
                </p>
              )}
            </div>
          )}
          {selected &&
            Object.entries(selected.inputs).map(([name, spec]) => (
              <InputField key={name} name={name} spec={spec} value={values[name]} onChange={(v) => setValues((s) => ({ ...s, [name]: v }))} />
            ))}
          <DialogFooter>
            <Button type="submit" disabled={!repo || busy || !pipelines?.length}>
              Run
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function InputField({ name, spec, value, onChange }: { name: string; spec: PipelineInput; value: string | boolean | undefined; onChange: (v: string | boolean) => void }) {
  const id = `input-${name}`
  if (spec.type === 'boolean')
    return (
      <label htmlFor={id} className="flex items-start gap-3 text-sm">
        <input id={id} type="checkbox" checked={value === true} onChange={(e) => onChange(e.target.checked)} className="mt-0.5 size-4 accent-foreground" />
        <span>
          <span className="font-medium">{name}</span>
          {spec.description && <span className="block text-muted-foreground">{spec.description}</span>}
        </span>
      </label>
    )
  return (
    <div className="grid gap-2">
      <Label htmlFor={id}>
        {name}
        {spec.required && <span className="text-muted-foreground">*</span>}
      </Label>
      {spec.type === 'choice' && spec.options?.length ? (
        <Select value={String(value ?? '')} onValueChange={onChange}>
          <SelectTrigger id={id} className="w-full">
            <SelectValue placeholder="Select…" />
          </SelectTrigger>
          <SelectContent>
            {spec.options.map((o) => (
              <SelectItem key={o} value={o}>
                {o}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : (
        <Input id={id} value={String(value ?? '')} onChange={(e) => onChange(e.target.value)} required={spec.required} inputMode={spec.type === 'number' ? 'decimal' : undefined} />
      )}
      {spec.description && <p className="text-xs text-muted-foreground">{spec.description}</p>}
    </div>
  )
}
