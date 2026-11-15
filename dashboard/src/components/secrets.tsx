import { useEffect, useState } from 'react'
import { XIcon } from 'lucide-react'
import { api, ago, type SecretInfo } from '@/api'
import { attempt } from '@/components/common'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

/** `scope` is `projects/<id>`, `repos/<id>`, or empty for server-wide secrets. */
type Kind = 'server' | 'project' | 'repo'

const REACH: Record<Kind, string> = {
  server: 'Every repository on this server gets them.',
  project: 'Every repository in the project gets them. They override server secrets with the same name.',
  repo: 'They override project and server secrets with the same name.',
}

export function SecretsHelp({ kind }: { kind: Kind }) {
  return (
    <>
      Encrypted at rest and never shown again. Workflows read them as <code className="font-mono whitespace-nowrap">{'${{ secrets.NAME }}'}</code>, <code className="font-mono">.moloko.yml</code> jobs list them under{' '}
      <code className="font-mono">secrets:</code>. {REACH[kind]}
    </>
  )
}

/** List, add, replace and delete the vault secrets of one scope. Values are write-only. */
export function SecretsEditor({ scope, active = true }: { scope: string; active?: boolean }) {
  const [list, setList] = useState<SecretInfo[] | null>(null)
  const [name, setName] = useState('')
  const [value, setValue] = useState('')
  const load = () => api.secrets(scope).then(setList).catch(() => setList([]))

  useEffect(() => {
    if (active) load()
  }, [active, scope])

  const save = async (e: React.FormEvent) => {
    e.preventDefault()
    const n = name.trim().toUpperCase()
    if (await attempt(() => api.putSecret(scope, n, value), `Saved ${n}`)) {
      setName('')
      setValue('')
      load()
    }
  }
  const remove = async (n: string) => {
    if (await attempt(() => api.deleteSecret(scope, n), `Deleted ${n}`)) load()
  }

  return (
    <>
      {list && list.length > 0 && (
        <ul className="divide-y border-y">
          {list.map((s) => (
            <li key={s.name} className="flex items-center gap-3 py-2">
              <code className="flex-1 truncate font-mono text-sm">{s.name}</code>
              <span className="text-xs text-muted-foreground">{ago(s.updated_at)}</span>
              <Button variant="ghost" size="icon-xs" className="text-muted-foreground" onClick={() => remove(s.name)} aria-label={`Delete ${s.name}`}>
                <XIcon />
              </Button>
            </li>
          ))}
        </ul>
      )}
      {list && list.length === 0 && <p className="text-sm text-muted-foreground">No secrets yet.</p>}

      <form onSubmit={save} className="grid gap-4 border-t pt-4">
        <div className="grid gap-2">
          <Label htmlFor={`secret-name-${scope}`}>Name</Label>
          <Input
            id={`secret-name-${scope}`}
            value={name}
            onChange={(e) => setName(e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, '_'))}
            placeholder="GITHUB_TOKEN"
            className="font-mono"
            required
          />
          {list?.some((s) => s.name === name.trim()) && <p className="text-xs text-muted-foreground">Saving replaces the current value.</p>}
        </div>
        <div className="grid gap-2">
          <Label htmlFor={`secret-value-${scope}`}>Value</Label>
          <textarea
            id={`secret-value-${scope}`}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            rows={3}
            required
            spellCheck={false}
            autoComplete="off"
            className="rounded-md border border-input bg-transparent px-2.5 py-2 font-mono text-sm outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 [-webkit-text-security:disc]"
          />
        </div>
        <div className="flex justify-end">
          <Button type="submit">Save secret</Button>
        </div>
      </form>
    </>
  )
}

export function SecretsDialog({ open, onOpenChange, scope, label, kind }: { open: boolean; onOpenChange: (o: boolean) => void; scope: string; label: string; kind: 'project' | 'repo' }) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Secrets · {label}</DialogTitle>
          <DialogDescription>
            <SecretsHelp kind={kind} />
          </DialogDescription>
        </DialogHeader>
        <SecretsEditor scope={scope} active={open} />
      </DialogContent>
    </Dialog>
  )
}
