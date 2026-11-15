import type { ReactNode } from 'react'
import { CheckIcon, DownloadIcon } from 'lucide-react'
import { api, type Download } from '@/api'
import { usePoll } from '@/hooks'
import { attempt, Card, ErrorNote, PageHeader } from '@/components/common'
import { SecretsEditor, SecretsHelp } from '@/components/secrets'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'

export function SettingsPage() {
  const downloads = usePoll(api.downloads, 2000, [])
  return (
    <>
      <PageHeader title="Settings" description="Applies to the whole server, every project and repository on it." />
      <div className="divide-y border-t">
        <Section title="Secrets" description={<SecretsHelp kind="server" />}>
          <Card className="p-4">
            <SecretsEditor scope="" />
          </Card>
        </Section>
        <Section title="Runner downloads" description="Jobs fetch these the first time they need them, which makes that first run slow. Download them here ahead of time instead.">
          {downloads.error && <ErrorNote>{downloads.error}</ErrorNote>}
          <Card>
            {!downloads.data ? (
              <div className="space-y-3 p-4">
                <Skeleton className="h-4 w-40" />
                <Skeleton className="h-3 w-72 max-w-full" />
              </div>
            ) : (
              <ul className="divide-y">
                {downloads.data.map((d) => (
                  <DownloadRow key={d.id} download={d} />
                ))}
              </ul>
            )}
          </Card>
        </Section>
      </div>
    </>
  )
}

/** Title and explanation on the left, the controls on the right. */
function Section({ title, description, children }: { title: string; description: ReactNode; children: ReactNode }) {
  return (
    <section className="grid gap-x-10 gap-y-4 py-8 lg:grid-cols-[18rem_1fr]">
      <div>
        <h2 className="text-sm font-semibold">{title}</h2>
        <p className="mt-1.5 text-sm leading-relaxed text-muted-foreground">{description}</p>
      </div>
      <div className="min-w-0">{children}</div>
    </section>
  )
}

function DownloadRow({ download: d }: { download: Download }) {
  const failed = d.state === 'missing' && d.message
  return (
    <li className="flex items-center gap-4 px-4 py-3.5">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">{d.name}</div>
        <div className="mt-0.5 text-xs text-muted-foreground">{d.detail}</div>
        {d.state === 'downloading' && (
          <div className="mt-2 flex items-center gap-2 font-mono text-xs text-muted-foreground">
            <span className="size-3 shrink-0 animate-spin rounded-full border-2 border-running/25 border-t-running" />
            <span className="truncate">{d.message ?? 'Starting…'}</span>
          </div>
        )}
        {failed && <div className="mt-2 rounded-md border border-destructive/25 bg-destructive/5 px-2 py-1 font-mono text-xs break-words text-destructive">{d.message}</div>}
      </div>
      {d.state === 'ready' ? (
        <span className="inline-flex h-6 shrink-0 items-center gap-1.5 rounded-full border border-success/25 bg-success/10 px-2.5 text-xs font-medium text-success">
          <CheckIcon className="size-3" strokeWidth={3} /> Downloaded
        </span>
      ) : (
        <Button variant="outline" size="sm" disabled={d.state === 'downloading'} onClick={() => attempt(() => api.startDownload(d.id))}>
          {d.state !== 'downloading' && <DownloadIcon />}
          {d.state === 'downloading' ? 'Downloading…' : failed ? 'Retry' : 'Download'}
        </Button>
      )}
    </li>
  )
}
