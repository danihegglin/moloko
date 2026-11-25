import { ago, api, type ProjectDetail, shortRef } from './api'
import { go, useNow, usePoll } from './hooks'
import { Badge, Diff, Err, Text } from './ui'

/** #/p/:pid/code/:repo?view=tree|blob|commits|commit|branches&ref=&path=&sha= */
export function CodeTab({ detail, repoId, query }: { detail: ProjectDetail; repoId?: number; query: URLSearchParams }) {
  const pid = detail.project.id
  const repo = detail.repos.find((r) => r.id === repoId) ?? detail.repos[0]
  if (!repo) return <p className="muted">No repositories. Add one in Settings.</p>
  const view = query.get('view') ?? 'tree'
  const ref = query.get('ref') ?? ''
  const path = query.get('path') ?? ''
  const link = (v: Record<string, string | undefined>) => {
    const p = new URLSearchParams()
    for (const [k, val] of Object.entries({ ref: ref || undefined, ...v })) if (val) p.set(k, val)
    return `#/p/${pid}/code/${repo.id}?${p}`
  }
  const clone = `${location.origin}/git/${detail.project.name}/${repo.name}.git`
  return (
    <>
      <section className="toolbar">
        {detail.repos.length > 1 && (
          <select value={repo.id} onChange={(e) => go(`#/p/${pid}/code/${e.target.value}`)}>
            {detail.repos.map((r) => (
              <option key={r.id} value={r.id}>
                {r.name}
              </option>
            ))}
          </select>
        )}
        <RefPicker repoId={repo.id} value={ref || repo.default_branch} onChange={(r) => go(link({ ref: r, view, path }))} />
        <nav className="tabs inline-tabs">
          <a href={link({ view: 'tree' })} className={view === 'tree' || view === 'blob' ? 'active' : ''}>
            Files
          </a>
          <a href={link({ view: 'commits' })} className={view === 'commits' || view === 'commit' ? 'active' : ''}>
            Commits
          </a>
          <a href={link({ view: 'branches' })} className={view === 'branches' ? 'active' : ''}>
            Branches
          </a>
        </nav>
        <code className="pull grow right">git clone {clone}</code>
      </section>
      {repo.mirror_mode !== 'none' && (
        <p className="muted small-text">
          {repo.mirror_mode === 'pull' ? 'Read-only mirror of' : repo.mirror_mode === 'push' ? 'Pushes to' : 'Two-way mirror with'} {repo.upstream_url}
        </p>
      )}
      {view === 'tree' && <Tree repoId={repo.id} gitRef={ref} path={path} link={link} />}
      {view === 'blob' && <Blob repoId={repo.id} gitRef={ref} path={path} link={link} />}
      {view === 'commits' && <Commits repoId={repo.id} gitRef={ref} path={path} link={link} />}
      {view === 'commit' && <CommitView repoId={repo.id} sha={query.get('sha') ?? ''} pid={pid} />}
      {view === 'branches' && <Branches repoId={repo.id} pid={pid} link={link} canReview={detail.role !== 'read' && repo.mirror_mode !== 'pull'} />}
    </>
  )
}

type Link = (v: Record<string, string | undefined>) => string

function RefPicker({ repoId, value, onChange }: { repoId: number; value: string; onChange: (r: string) => void }) {
  const b = usePoll(() => api.branches(repoId), 60000, [repoId])
  const names = (b.data?.refs ?? []).map((r) => shortRef(r.name))
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}>
      {!names.includes(value) && <option>{value}</option>}
      {names.map((n) => (
        <option key={n}>{n}</option>
      ))}
    </select>
  )
}

function Crumbs({ path, link, view }: { path: string; link: Link; view: string }) {
  const parts = path.split('/').filter(Boolean)
  return (
    <div className="crumbs">
      <a className="link" href={link({ view: 'tree' })}>
        root
      </a>
      {parts.map((p, i) => (
        <span key={i}>
          {' / '}
          {i === parts.length - 1 && view === 'blob' ? (
            <strong>{p}</strong>
          ) : (
            <a className="link" href={link({ view: 'tree', path: parts.slice(0, i + 1).join('/') })}>
              {p}
            </a>
          )}
        </span>
      ))}
    </div>
  )
}

function Tree({ repoId, gitRef, path, link }: { repoId: number; gitRef: string; path: string; link: Link }) {
  const t = usePoll(() => api.tree(repoId, gitRef || undefined, path || undefined), 60000, [repoId, gitRef, path])
  if (t.error) return <p className="muted">{t.error.includes('no commits') || t.error.includes('not found') ? 'Empty repository. Push something to get started.' : t.error}</p>
  if (!t.data) return <p className="muted">Loading…</p>
  const entries = [...t.data.entries].sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name) : a.type === 'tree' ? -1 : 1))
  return (
    <>
      <Crumbs path={path} link={link} view="tree" />
      <table className="runs files">
        <tbody>
          {entries.map((e) => (
            <tr key={e.path} onClick={() => e.type !== 'commit' && go(link({ view: e.type === 'tree' ? 'tree' : 'blob', path: e.path }))}>
              <td className="num">{e.type === 'tree' ? '📁' : e.type === 'commit' ? '🔗' : '📄'}</td>
              <td>{e.name}</td>
              <td className="muted right">{e.size == null ? '' : e.size < 1024 ? `${e.size} B` : `${(e.size / 1024).toFixed(1)} KB`}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {t.data.readme && (
        <section className="panel readme">
          <div className="muted small-text">{t.data.readme.path}</div>
          <Text body={t.data.readme.content} />
        </section>
      )}
    </>
  )
}

function Blob({ repoId, gitRef, path, link }: { repoId: number; gitRef: string; path: string; link: Link }) {
  const b = usePoll(() => api.blob(repoId, gitRef || undefined, path), 60000, [repoId, gitRef, path])
  return (
    <>
      <Crumbs path={path} link={link} view="blob" />
      <Err text={b.error} />
      {b.data &&
        (b.data.binary || b.data.content == null ? (
          <p className="muted">Binary file, {b.data.size} bytes.</p>
        ) : (
          <table className="diff blob">
            <tbody>
              {b.data.content.split('\n').map((l, i) => (
                <tr key={i}>
                  <td className="ln">{i + 1}</td>
                  <td className="code">{l}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ))}
      <p>
        <a className="link small-text" href={link({ view: 'commits', path })}>
          History of this file
        </a>
      </p>
    </>
  )
}

function Commits({ repoId, gitRef, path, link }: { repoId: number; gitRef: string; path: string; link: Link }) {
  const c = usePoll(() => api.commits(repoId, gitRef || undefined, path || undefined), 30000, [repoId, gitRef, path])
  const now = useNow(30000)
  return (
    <>
      {path && <p className="muted">History of {path}</p>}
      <Err text={c.error} />
      <table className="runs">
        <tbody>
          {c.data?.map(({ commit, run }) => (
            <tr key={commit.sha} onClick={() => go(link({ view: 'commit', sha: commit.sha }))}>
              <td>
                <code className="sha">{commit.sha.slice(0, 7)}</code>
              </td>
              <td>{commit.subject}</td>
              <td>{run && <Badge status={run.status} />}</td>
              <td className="muted">{commit.author}</td>
              <td className="muted right">{ago(commit.date, now)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  )
}

function CommitView({ repoId, sha, pid }: { repoId: number; sha: string; pid: number }) {
  const c = usePoll(() => api.commit(repoId, sha), 60000, [repoId, sha])
  if (c.error) return <p className="error">{c.error}</p>
  if (!c.data) return <p className="muted">Loading…</p>
  const { commit, run, diff, files } = c.data
  return (
    <>
      <section className="run-head">
        <h1>{commit.subject}</h1>
        {commit.body && <Text body={commit.body} pid={pid} />}
        <p className="muted">
          {commit.author} · {new Date(commit.date).toLocaleString()} · <code className="sha">{commit.sha}</code>
          {run && (
            <a className="link" href={`#/runs/${run.id}`}>
              {' '}
              <Badge status={run.status} /> run #{run.id}
            </a>
          )}
        </p>
        <p className="muted small-text">
          {files.length} files · +{files.reduce((n, f) => n + (f.added ?? 0), 0)} −{files.reduce((n, f) => n + (f.removed ?? 0), 0)}
        </p>
      </section>
      <Diff diff={diff} />
    </>
  )
}

function Branches({ repoId, pid, link, canReview }: { repoId: number; pid: number; link: Link; canReview: boolean }) {
  const b = usePoll(() => api.branches(repoId), 15000, [repoId])
  const now = useNow(30000)
  return (
    <>
      <Err text={b.error} />
      <table className="runs">
        <tbody>
          {b.data?.refs.map((r) => {
            const name = shortRef(r.name)
            return (
              <tr key={r.name} onClick={() => go(link({ view: 'tree', ref: name }))}>
                <td>
                  <strong>{name}</strong>
                  {name === b.data!.default_branch && <span className="label">default</span>}
                  {r.name.startsWith('refs/tags/') && <span className="label">tag</span>}
                  {r.mirror && r.mirror !== 'in_sync' && <span className={`label ${r.mirror}`}>{r.mirror}</span>}
                </td>
                <td className="muted">{r.subject}</td>
                <td>{r.run && <Badge status={r.run.status} />}</td>
                <td className="muted right">{ago(r.date, now)}</td>
                <td className="right">
                  {canReview && r.name.startsWith('refs/heads/') && name !== b.data!.default_branch && (
                    <button
                      className="secondary small"
                      onClick={(e) => {
                        e.stopPropagation()
                        go(`#/p/${pid}/reviews/new?repo=${repoId}&branch=${encodeURIComponent(name)}`)
                      }}
                    >
                      Open review
                    </button>
                  )}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </>
  )
}
