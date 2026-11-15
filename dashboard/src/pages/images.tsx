import { api, ago } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { CodeLine, CopyButton, EmptyState, Muted, PageHeader } from '@/components/common'

export function ImagesPage() {
  const images = usePoll(api.images, 5000, [])
  const now = useNow(10000)
  if (images.error) return <p className="text-sm text-destructive">{images.error}</p>
  if (!images.data) return <Muted>Loading…</Muted>
  const { registry, repositories } = images.data
  return (
    <>
      <PageHeader
        title="Images"
        description={
          <span className="inline-flex items-center gap-1">
            Built-in registry at <code className="font-mono text-foreground">{registry}</code>
            <CopyButton value={registry} label="Copied registry address" />
          </span>
        }
      />
      {repositories.length === 0 ? (
        <EmptyState title="No images yet">
          In a job: <code className="font-mono">docker push $MOLOKO_IMAGE:$MOLOKO_SHA</code> after logging in with <code className="font-mono">$MOLOKO_REGISTRY_TOKEN</code>.
        </EmptyState>
      ) : (
        <div className="space-y-12">
          {repositories.map((repo) => (
            <section key={repo.name}>
              <div className="mb-3 flex items-baseline gap-3">
                <h2 className="font-mono text-sm font-medium">{repo.name}</h2>
                <span className="text-sm text-muted-foreground">
                  {repo.tags.length} {repo.tags.length === 1 ? 'tag' : 'tags'}
                </span>
              </div>
              <ul className="divide-y border-y">
                {repo.tags.map((t) => (
                  <li key={t.tag} className="flex items-center gap-4 py-3">
                    <div className="w-40 shrink-0">
                      <div className="truncate font-mono text-sm">{t.tag.length > 24 ? t.tag.slice(0, 12) : t.tag}</div>
                      <div className="font-mono text-xs text-muted-foreground">{t.digest.slice(7, 19)}</div>
                    </div>
                    <CodeLine value={`docker pull ${registry}/${repo.name}:${t.tag}`} className="min-w-0 flex-1" />
                    <span className="hidden w-20 shrink-0 text-right text-xs text-muted-foreground sm:block">{ago(t.updated_at, now)}</span>
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </div>
      )}
    </>
  )
}
