import { type ComponentType, type ReactNode, useState } from 'react'
import { toast } from 'sonner'
import { CheckIcon, CircleDashedIcon, CircleMinusIcon, CircleSlashIcon, CopyIcon, GitBranchIcon, GitCommitHorizontalIcon, TagIcon, UserIcon, XIcon } from 'lucide-react'
import type { Job, Status } from '@/api'
import { shortRef } from '@/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'

export const STATUS_LABEL: Record<Status, string> = {
  queued: 'Queued',
  running: 'Running',
  success: 'Passed',
  failed: 'Failed',
  skipped: 'Skipped',
  cancelled: 'Cancelled',
}

/** Status at a glance: green check = passed, red cross = failed, blue spinner = running, grey = waiting or not run. */
export function StatusIcon({ status, className }: { status: Status; className?: string }) {
  const label = STATUS_LABEL[status]
  const box = cn('inline-grid size-4 shrink-0 place-items-center rounded-full', className)
  if (status === 'success' || status === 'failed') {
    const Icon = status === 'success' ? CheckIcon : XIcon
    return (
      <span role="img" aria-label={label} title={label} className={cn(box, 'text-white', status === 'success' ? 'bg-success' : 'bg-destructive')}>
        <Icon className="size-[62%]" strokeWidth={3.5} />
      </span>
    )
  }
  if (status === 'running') return <span role="img" aria-label={label} title={label} className={cn(box, 'animate-spin border-2 border-running/25 border-t-running')} />
  const Icon = status === 'queued' ? CircleDashedIcon : status === 'cancelled' ? CircleSlashIcon : CircleMinusIcon
  return (
    <span role="img" aria-label={label} title={label} className={cn(box, 'text-muted-foreground', status !== 'queued' && 'opacity-70')}>
      <Icon className="size-full" />
    </span>
  )
}

const TONE: Record<Status, string> = {
  success: 'border-success/25 bg-success/10 text-success',
  failed: 'border-destructive/25 bg-destructive/10 text-destructive',
  running: 'border-running/25 bg-running/10 text-running',
  queued: 'bg-muted text-muted-foreground',
  skipped: 'bg-muted text-muted-foreground',
  cancelled: 'bg-muted text-muted-foreground',
}

export function StatusBadge({ status, className }: { status: Status; className?: string }) {
  return (
    <span className={cn('inline-flex h-6 shrink-0 items-center gap-1.5 rounded-full border px-2.5 text-xs font-medium', TONE[status], className)}>
      <span className={cn('size-1.5 rounded-full bg-current', status === 'running' && 'animate-pulse')} />
      {STATUS_LABEL[status]}
    </span>
  )
}

/** A small neutral pill for counts and labels. */
export function Pill({ children, className }: { children: ReactNode; className?: string }) {
  return <span className={cn('inline-flex h-5 shrink-0 items-center gap-1 rounded-full border bg-muted/60 px-2 text-[11px] font-medium text-muted-foreground', className)}>{children}</span>
}

/** One segment per job: green = passed, red = failed, pulsing blue = running, faint = not there yet. */
export function JobBar({ jobs, className }: { jobs: Job[]; className?: string }) {
  return (
    <div className={cn('flex h-1.5 gap-0.5', className)}>
      {jobs.map((j) => (
        <span
          key={j.id}
          title={`${j.name}: ${STATUS_LABEL[j.status].toLowerCase()}`}
          className={cn(
            'min-w-1 flex-1 rounded-full',
            j.status === 'success' && 'bg-success',
            j.status === 'failed' && 'bg-destructive',
            j.status === 'running' && 'animate-pulse bg-running',
            (j.status === 'queued' || j.status === 'skipped' || j.status === 'cancelled') && 'bg-foreground/10',
          )}
        />
      ))}
    </div>
  )
}

/** A branch or tag name with its icon. */
export function RefChip({ gitRef, className }: { gitRef: string; className?: string }) {
  const Icon = gitRef.startsWith('refs/tags/') ? TagIcon : GitBranchIcon
  return (
    <span title={shortRef(gitRef)} className={cn('inline-flex h-6 max-w-full min-w-0 items-center gap-1.5 rounded-md border bg-muted/50 px-2 font-mono text-xs', className)}>
      <Icon className="size-3 shrink-0 text-muted-foreground" />
      <span className="truncate">{shortRef(gitRef)}</span>
    </span>
  )
}

export function Sha({ sha, className }: { sha: string; className?: string }) {
  return (
    <span title={sha} className={cn('inline-flex items-center gap-1 font-mono text-xs text-muted-foreground', className)}>
      <GitCommitHorizontalIcon className="size-3.5 shrink-0" />
      {sha.slice(0, 7)}
    </span>
  )
}

const TRIGGER: Record<string, [ComponentType<{ className?: string }>, string]> = {
  push: [GitCommitHorizontalIcon, 'Push'],
  manual: [UserIcon, 'Manual'],
}

/** What started a run. */
export function Trigger({ trigger, className }: { trigger: string; className?: string }) {
  const [Icon, label] = TRIGGER[trigger] ?? [GitCommitHorizontalIcon, trigger]
  return (
    <span className={cn('inline-flex items-center gap-1', className)}>
      <Icon className="size-3.5 shrink-0" />
      {label}
    </span>
  )
}

/** The surface lists and panels sit on. */
export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn('overflow-hidden rounded-xl border bg-card shadow-xs', className)}>{children}</div>
}

export function PageHeader({ title, description, children }: { title: ReactNode; description?: ReactNode; children?: ReactNode }) {
  return (
    <div className="mb-8 flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
      <div className="min-w-0">
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
        {description && <p className="mt-1.5 max-w-2xl text-sm text-muted-foreground">{description}</p>}
      </div>
      {children && <div className="flex flex-wrap items-center gap-2">{children}</div>}
    </div>
  )
}

export function EmptyState({ icon: Icon, title, children, action }: { icon?: ComponentType<{ className?: string }>; title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center rounded-xl border border-dashed bg-card/50 px-6 py-16 text-center">
      {Icon && (
        <div className="mb-4 grid size-10 place-items-center rounded-lg border bg-card shadow-xs">
          <Icon className="size-5 text-muted-foreground" />
        </div>
      )}
      <p className="text-sm font-medium">{title}</p>
      {children && <div className="mx-auto mt-1.5 max-w-md text-sm text-muted-foreground">{children}</div>}
      {action && <div className="mt-5">{action}</div>}
    </div>
  )
}

/** Placeholder rows while a list loads. */
export function ListSkeleton({ rows = 4 }: { rows?: number }) {
  return (
    <Card aria-busy="true">
      <ul className="divide-y">
        {Array.from({ length: rows }, (_, i) => (
          <li key={i} className="flex items-center gap-4 px-4 py-4">
            <Skeleton className="size-4 rounded-full" />
            <div className="flex-1 space-y-2">
              <Skeleton className="h-3.5 w-48" />
              <Skeleton className="h-3 w-72 max-w-full" />
            </div>
            <Skeleton className="hidden h-3.5 w-16 sm:block" />
          </li>
        ))}
      </ul>
    </Card>
  )
}

export function ErrorNote({ children }: { children: ReactNode }) {
  return <p className="mb-4 rounded-lg border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm text-destructive">{children}</p>
}

export function CopyButton({ value, label = 'Copied', className }: { value: string | (() => string); label?: string; className?: string }) {
  const [done, setDone] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(typeof value === 'string' ? value : value())
      setDone(true)
      toast(label)
      setTimeout(() => setDone(false), 1500)
    } catch {
      toast.error('Could not copy; select the text instead')
    }
  }
  return (
    <Button variant="ghost" size="icon-xs" className={cn('text-muted-foreground', className)} onClick={copy} aria-label="Copy" title="Copy">
      {done ? <CheckIcon /> : <CopyIcon />}
    </Button>
  )
}

/** A one-line code snippet with a copy button. */
export function CodeLine({ value, className }: { value: string; className?: string }) {
  return (
    <div className={cn('flex min-w-0 items-center gap-1 rounded-md border bg-muted/40 py-0.5 pr-0.5 pl-2.5', className)}>
      <code className="min-w-0 flex-1 truncate font-mono text-xs select-all">{value}</code>
      <CopyButton value={value} />
    </div>
  )
}

export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  action,
  destructive = true,
  onConfirm,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description: ReactNode
  action: string
  destructive?: boolean
  onConfirm: () => void
}) {
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction variant={destructive ? 'destructive' : 'default'} onClick={onConfirm}>
            {action}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}

/** Run `f`, show its error as a toast, and report whether it succeeded. */
export async function attempt(f: () => Promise<unknown>, success?: string) {
  try {
    await f()
    if (success) toast(success)
    return true
  } catch (err) {
    toast.error((err as Error).message)
    return false
  }
}
