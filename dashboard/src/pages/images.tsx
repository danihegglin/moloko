import { BoxIcon } from 'lucide-react'
import { api, ago } from '@/api'
import { useNow, usePoll } from '@/hooks'
import { Card, CodeLine, CopyButton, EmptyState, ErrorNote, ListSkeleton, PageHeader, Pill } from '@/components/common'

export function ImagesPage() {
  const images = usePoll(api.images, 5000, [])
  const now = useNow(10000)
  const registry = images.data?.registry
  return (
    <>
      <PageHeader
        title="Images"
        description={
          registry ? (
            <span className="inline-flex flex-wrap items-center gap-1">
              Container images pushed by jobs to the built-in registry at <code className="font-mono text-foreground">{registry}</code>
              <CopyButton value={registry} label="Copied registry address" />
            </span>
          ) : (
            'Container images pushed by jobs to the built-in registry.'
          )
        }
      />
      {images.error && <ErrorNote>{images.error}</ErrorNote>}
      {!images.data ? (
        <ListSkeleton rows={3} />
      ) : images.data.repositories.length === 0 ? (
        <EmptyState icon={BoxIcon} title="No images yet">
          In a job: <code className="font-mono">docker push $MOLOKO_IMAGE:$MOLOKO_SHA</code> after logging in with <code className="font-mono">$MOLOKO_REGISTRY_TOKEN</code>.
        </EmptyState>
      ) : (
        <div className="space-y-6">
          {images.data.repositories.map((repo) => (
            <Card key={repo.name}>
              <div className="flex items-center gap-3 border-b bg-muted/40 px-4 py-2.5">
                <BoxIcon className="size-4 shrink-0 text-muted-foreground" />
                <h2 className="truncate font-mono text-sm font-semibold">{repo.name}</h2>
                <Pill>
                  {repo.tags.length} {repo.tags.length === 1 ? 'tag' : 'tags'}
                </Pill>
              </div>
              <ul className="divide-y">
                {repo.tags.map((t) => (
                  <li key={t.tag} className="flex items-center gap-4 px-4 py-3">
                    <div className="w-36 shrink-0">
                      <div className="truncate font-mono text-sm font-medium" title={t.tag}>
                        {t.tag.length > 24 ? t.tag.slice(0, 12) : t.tag}
                      </div>
                      <div className="mt-0.5 font-mono text-xs text-muted-foreground" title={t.digest}>
                        {t.digest.slice(7, 19)}
                      </div>
                    </div>
                    <CodeLine value={`docker pull ${registry}/${repo.name}:${t.tag}`} className="min-w-0 flex-1" />
                    <span className="hidden w-20 shrink-0 text-right text-xs text-muted-foreground tabular-nums sm:block">{ago(t.updated_at, now)}</span>
                  </li>
                ))}
              </ul>
            </Card>
          ))}
        </div>
      )}
    </>
  )
}
