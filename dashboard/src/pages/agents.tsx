import { useState } from 'react'
import { MoreHorizontalIcon } from 'lucide-react'
import { api, ago, type AgentRow, type AgentSecret } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { cn } from '@/lib/utils'
import { attempt, ConfirmDialog, CopyButton, EmptyState, Muted, PageHeader } from '@/components/common'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

export function AgentsPage() {
  const [version, setVersion] = useState(0)
  const agents = usePoll(api.agents, 3000, [version])
  const [adding, setAdding] = useState(false)
  const [secret, setSecret] = useState<{ name: string; secret: AgentSecret; rotated?: boolean } | null>(null)
  const now = useNow(5000)
  const refresh = () => setVersion((v) => v + 1)
  const online = agents.data?.filter((a) => a.online).length ?? 0

  return (
    <>
      <PageHeader
        title="Agents"
        description={agents.data?.length ? `${online} of ${agents.data.length} connected.` : 'Deploy agents run on your servers and connect out to moloko.'}
      >
        <Button onClick={() => setAdding(true)}>Add agent</Button>
      </PageHeader>
      <AddAgentDialog
        open={adding}
        onOpenChange={setAdding}
        onCreated={(name, s) => {
          setSecret({ name, secret: s })
          refresh()
        }}
      />
      {secret && <SecretDialog {...secret} onClose={() => setSecret(null)} />}

      {agents.error && <p className="mb-4 text-sm text-destructive">{agents.error}</p>}
      {!agents.data ? (
        <Muted>Loading…</Muted>
      ) : agents.data.length === 0 ? (
        <EmptyState title="No agents yet">
          Add an agent, run the join command on a server, then target it from a pipeline with <code className="font-mono">agent: &lt;label&gt;</code>.
        </EmptyState>
      ) : (
        <ul className="divide-y border-y">
          {agents.data.map((row) => (
            <AgentRowItem
              key={row.agent.id}
              row={row}
              now={now}
              onDeleted={refresh}
              onRotated={(s) => {
                setSecret({ name: row.agent.name, secret: s, rotated: true })
                refresh()
              }}
            />
          ))}
        </ul>
      )}
    </>
  )
}

function AgentRowItem({ row, now, onDeleted, onRotated }: { row: AgentRow; now: number; onDeleted: () => void; onRotated: (s: AgentSecret) => void }) {
  const { agent, online, presence } = row
  const [dialog, setDialog] = useState<'rotate' | 'delete' | null>(null)
  const close = (open: boolean) => !open && setDialog(null)
  return (
    <li className="flex items-center gap-4 py-3.5">
      <span className={cn('size-2 shrink-0 rounded-full', online ? 'bg-foreground' : 'border border-muted-foreground')} title={online ? 'Connected' : 'Offline'} />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2 text-sm">
          <span className="font-medium">{agent.name}</span>
          {agent.labels.length > 0 && <span className="truncate font-mono text-xs text-muted-foreground">{agent.labels.join(', ')}</span>}
        </div>
        <div className="mt-0.5 truncate text-xs text-muted-foreground">
          {presence ? `${presence.hostname} · ${presence.os} · v${presence.version}` : agent.last_seen ? `Last seen ${ago(agent.last_seen, now)}` : 'Never connected'}
        </div>
      </div>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" size="icon-sm" className="text-muted-foreground" aria-label="More">
            <MoreHorizontalIcon />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-40">
          <DropdownMenuItem onSelect={() => setDialog('rotate')}>New join code</DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem variant="destructive" onSelect={() => setDialog('delete')}>
            Delete
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <ConfirmDialog
        open={dialog === 'rotate'}
        onOpenChange={close}
        title={`New join code for ${agent.name}?`}
        description="The current code stops working and the agent is disconnected."
        action="Issue new code"
        destructive={false}
        onConfirm={async () => {
          try {
            onRotated(await api.rotateJoin(agent.id))
          } catch (err) {
            await attempt(() => Promise.reject(err))
          }
        }}
      />
      <ConfirmDialog
        open={dialog === 'delete'}
        onOpenChange={close}
        title={`Delete ${agent.name}?`}
        description="Its token stops working immediately."
        action="Delete agent"
        onConfirm={() => attempt(() => api.deleteAgent(agent.id), `Deleted ${agent.name}`).then(onDeleted)}
      />
    </li>
  )
}

function AddAgentDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange: (o: boolean) => void; onCreated: (name: string, s: AgentSecret) => void }) {
  const [name, setName] = useState('')
  const [labels, setLabels] = useState('')
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    const list = labels.split(',').map((l) => l.trim()).filter(Boolean)
    let s: AgentSecret | undefined
    if (await attempt(async () => (s = await api.createAgent(name, list)))) {
      onOpenChange(false)
      onCreated(name, s!)
      setName('')
      setLabels('')
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>Add agent</DialogTitle>
            <DialogDescription>You'll get a one-time join code to run on the server.</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="agent-name">Name</Label>
            <Input id="agent-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="web1" autoFocus required />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="agent-labels">Labels</Label>
            <Input id="agent-labels" value={labels} onChange={(e) => setLabels(e.target.value)} placeholder="prod, eu" />
          </div>
          <DialogFooter>
            <Button type="submit">Create</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function SecretDialog({ name, secret, rotated, onClose }: { name: string; secret: AgentSecret; rotated?: boolean; onClose: () => void }) {
  const wsUrl = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/agents/connect`
  const command = secret.join ? `MOLOKO_JOIN=${secret.join} moloko agent` : `MOLOKO_AGENT_TOKEN=${secret.token} moloko agent --server ${wsUrl}`
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{rotated ? `New join code for ${name}` : `${name} is ready`}</DialogTitle>
          <DialogDescription>Copy it now; it won't be shown again. Run this on the server:</DialogDescription>
        </DialogHeader>
        <div className="relative min-w-0 rounded-md bg-[#0a0a0a] p-4 pr-11">
          <pre className="font-mono text-xs break-all whitespace-pre-wrap text-neutral-200">{command}</pre>
          <CopyButton value={command} className="absolute top-2 right-2 text-neutral-500 hover:bg-white/10 hover:text-white" />
        </div>
        <DialogFooter>
          <Button onClick={onClose}>Done</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
