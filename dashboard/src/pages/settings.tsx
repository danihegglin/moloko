import { api, type Download } from '@/api'
import { usePoll } from '@/hooks'
import { attempt, Muted, PageHeader } from '@/components/common'
import { SecretsEditor, SecretsHelp } from '@/components/secrets'
import { Button } from '@/components/ui/button'

export function SettingsPage() {
  const downloads = usePoll(api.downloads, 2000, [])
  return (
    <>
      <PageHeader title="Settings" />
      <section className="mb-12">
        <h2 className="font-semibold tracking-tight">Secrets</h2>
        <p className="mt-1.5 mb-4 max-w-xl text-sm text-muted-foreground">
          <SecretsHelp kind="server" />
        </p>
        <div className="grid max-w-xl gap-4">
          <SecretsEditor scope="" />
        </div>
      </section>
      <section>
        <h2 className="font-semibold tracking-tight">Runner downloads</h2>
        <p className="mt-1.5 mb-4 max-w-xl text-sm text-muted-foreground">
          Jobs fetch these the first time they need them, which makes that first run slow. Download them here ahead of time instead.
        </p>
        {downloads.error && <p className="mb-4 text-sm text-destructive">{downloads.error}</p>}
        {!downloads.data ? (
          <Muted>Loading…</Muted>
        ) : (
          <ul className="divide-y border-y">
            {downloads.data.map((d) => (
              <DownloadRow key={d.id} download={d} />
            ))}
          </ul>
        )}
      </section>
    </>
  )
}

function DownloadRow({ download: d }: { download: Download }) {
  const failed = d.state === 'missing' && d.message
  return (
    <li className="flex items-center gap-4 py-3">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">{d.name}</div>
        <div className="text-xs text-muted-foreground">{d.detail}</div>
        {d.state === 'downloading' && <div className="mt-1 truncate font-mono text-xs text-muted-foreground">{d.message ?? 'Starting…'}</div>}
        {failed && <div className="mt-1 font-mono text-xs break-words text-destructive">{d.message}</div>}
      </div>
      {d.state === 'ready' ? (
        <span className="text-sm text-muted-foreground">Downloaded</span>
      ) : (
        <Button variant="outline" size="sm" disabled={d.state === 'downloading'} onClick={() => attempt(() => api.startDownload(d.id))}>
          {d.state === 'downloading' ? 'Downloading…' : failed ? 'Retry' : 'Download'}
        </Button>
      )}
    </li>
  )
}
