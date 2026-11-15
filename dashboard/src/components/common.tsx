import { type ReactNode, useState } from 'react'
import { toast } from 'sonner'
import { CheckIcon, CopyIcon } from 'lucide-react'
import type { Job, Status } from '@/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
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

const LABEL: Record<Status, string> = {
  queued: 'Queued',
  running: 'Running',
  success: 'Passed',
  failed: 'Failed',
  skipped: 'Skipped',
  cancelled: 'Cancelled',
}

/** Monochrome status: solid = passed, ring = running, hollow = waiting/skipped, red = failed. */
export function StatusDot({ status, className }: { status: Status; className?: string }) {
  return (
    <span
      aria-label={LABEL[status]}
      title={LABEL[status]}
      className={cn(
        'relative inline-flex size-2 shrink-0 rounded-full',
        status === 'success' && 'bg-foreground',
        status === 'failed' && 'bg-destructive',
        status === 'running' && 'border border-foreground',
        status === 'queued' && 'border border-muted-foreground',
        (status === 'skipped' || status === 'cancelled') && 'border border-dashed border-muted-foreground/70',
        className,
      )}
    >
      {status === 'running' && <span className="absolute inset-0 animate-ping rounded-full bg-foreground/40" />}
    </span>
  )
}

/** One segment per job: filled = passed, red = failed, pulsing = running, faint = not there yet. */
export function JobBar({ jobs, className }: { jobs: Job[]; className?: string }) {
  return (
    <div className={cn('flex h-1 gap-0.5', className)}>
      {jobs.map((j) => (
        <span
          key={j.id}
          title={`${j.name}: ${LABEL[j.status].toLowerCase()}`}
          className={cn(
            'min-w-1 flex-1 rounded-full',
            j.status === 'success' && 'bg-foreground',
            j.status === 'failed' && 'bg-destructive',
            j.status === 'running' && 'animate-pulse bg-foreground/50',
            (j.status === 'queued' || j.status === 'skipped' || j.status === 'cancelled') && 'bg-border',
          )}
        />
      ))}
    </div>
  )
}

export function StatusLabel({ status }: { status: Status }) {
  return (
    <span className={cn('inline-flex items-center gap-2 text-sm', status === 'failed' ? 'text-destructive' : 'text-muted-foreground')}>
      <StatusDot status={status} />
      {LABEL[status]}
    </span>
  )
}

export function PageHeader({ title, description, children }: { title: string; description?: ReactNode; children?: ReactNode }) {
  return (
    <div className="mb-10 flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
      <div className="min-w-0">
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
        {description && <p className="mt-1.5 max-w-xl text-sm text-muted-foreground">{description}</p>}
      </div>
      {children && <div className="flex flex-wrap items-center gap-2">{children}</div>}
    </div>
  )
}

export function EmptyState({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="rounded-md border border-dashed px-6 py-20 text-center">
      <p className="text-sm font-medium">{title}</p>
      <div className="mx-auto mt-2 max-w-md text-sm text-muted-foreground">{children}</div>
    </div>
  )
}

export function Muted({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cn('text-sm text-muted-foreground', className)}>{children}</p>
}

export function CopyButton({ value, label = 'Copied', className }: { value: string; label?: string; className?: string }) {
  const [done, setDone] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value)
      setDone(true)
      toast(label)
      setTimeout(() => setDone(false), 1500)
    } catch {
      toast.error('Could not copy; select the text instead')
    }
  }
  return (
    <Button variant="ghost" size="icon-xs" className={cn('text-muted-foreground', className)} onClick={copy} aria-label="Copy">
      {done ? <CheckIcon /> : <CopyIcon />}
    </Button>
  )
}

/** A one-line code snippet with a copy button. */
export function CodeLine({ value, className }: { value: string; className?: string }) {
  return (
    <div className={cn('flex min-w-0 items-center gap-1 rounded-md border py-0.5 pr-0.5 pl-2.5', className)}>
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
