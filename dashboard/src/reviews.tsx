import { useState } from 'react'
import { ago, api, type ProjectDetail, type ReviewComment, shortRef } from './api'
import { go, useAction, useNow, usePoll } from './hooks'
import { Badge, Diff, Err, Text } from './ui'

export function ReviewsTab({ detail }: { detail: ProjectDetail }) {
  const pid = detail.project.id
  const [state, setState] = useState('open')
  const reviews = usePoll(() => api.reviews(pid, state), 10000, [pid, state])
  const now = useNow(30000)
  return (
    <>
      <section className="toolbar">
        <select value={state} onChange={(e) => setState(e.target.value)}>
          <option value="open">Open</option>
          <option value="merged">Merged</option>
          <option value="closed">Closed</option>
          <option value="all">All</option>
        </select>
        {detail.role !== 'read' && <button onClick={() => go(`#/p/${pid}/reviews/new`)}>+ Review</button>}
      </section>
      <Err text={reviews.error} />
      {reviews.data?.length === 0 && (
        <div className="empty">
          <p>No reviews.</p>
          <p className="muted">Push a branch and open a review. Branches named after an issue, like {detail.project.key.toLowerCase()}-12-fix-login, link to that issue.</p>
        </div>
      )}
      <table className="runs">
        <tbody>
          {reviews.data?.map((r) => (
            <tr key={r.id} onClick={() => go(`#/p/${pid}/reviews/${r.number}`)}>
              <td className="num">!{r.number}</td>
              <td>
                <Badge status={r.state} />
                {r.queue_state && <span className="label">queue: {r.queue_state}</span>}
              </td>
              <td>
                <strong>{r.title}</strong>
                <span className="muted">
                  {' '}
                  · {r.repo} · {r.source_branch} → {r.target_branch}
                </span>
                {r.issue_key && <span className="label">{r.issue_key}</span>}
              </td>
              <td className="muted">
                {r.author_kind === 'bot' ? '🤖 ' : ''}
                {r.author}
              </td>
              <td className="muted right">{ago(r.updated_at, now)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  )
}

export function NewReview({ detail, query }: { detail: ProjectDetail; query: URLSearchParams }) {
  const pid = detail.project.id
  const repos = detail.repos.filter((r) => r.mirror_mode !== 'pull')
  const [repoId, setRepoId] = useState(Number(query.get('repo')) || repos[0]?.id || 0)
  const [source, setSource] = useState(query.get('branch') ?? '')
  const [target, setTarget] = useState('')
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  const [draft, setDraft] = useState(false)
  const branches = usePoll(() => api.branches(repoId), 60000, [repoId])
  const a = useAction()
  if (repos.length === 0) return <p className="muted">This project has no repository that accepts reviews. Pull mirrors are read-only.</p>
  const names = (branches.data?.refs ?? []).filter((r) => r.name.startsWith('refs/heads/')).map((r) => shortRef(r.name))
  const def = branches.data?.default_branch ?? 'main'
  return (
    <form
      className="panel form"
      onSubmit={(e) => {
        e.preventDefault()
        a.run(async () => {
          const r = await api.createReview(pid, { repo_id: repoId, source_branch: source, target_branch: target || def, title: title || undefined, body, draft })
          go(`#/p/${pid}/reviews/${r.number}`)
        })
      }}
    >
      <h2>New review</h2>
      <div className="inline">
        <select value={repoId} onChange={(e) => setRepoId(Number(e.target.value))}>
          {repos.map((r) => (
            <option key={r.id} value={r.id}>
              {r.name}
            </option>
          ))}
        </select>
        <select value={source} onChange={(e) => setSource(e.target.value)} required>
          <option value="">branch…</option>
          {names
            .filter((n) => n !== def)
            .map((n) => (
              <option key={n}>{n}</option>
            ))}
        </select>
        <span className="muted">into</span>
        <select value={target || def} onChange={(e) => setTarget(e.target.value)}>
          {names.map((n) => (
            <option key={n}>{n}</option>
          ))}
        </select>
      </div>
      <input className="wide" placeholder="Title (defaults to the last commit)" value={title} onChange={(e) => setTitle(e.target.value)} />
      <textarea placeholder="Description. Write 'fixes KEY-12' to close an issue on merge." value={body} onChange={(e) => setBody(e.target.value)} />
      <div className="inline">
        <label className="check">
          <input type="checkbox" checked={draft} onChange={(e) => setDraft(e.target.checked)} /> Draft
        </label>
        <button type="submit" disabled={a.busy || !source}>
          Open review
        </button>
        <Err text={a.error ?? branches.error} />
      </div>
    </form>
  )
}

export function ReviewPage({ detail, number, query }: { detail: ProjectDetail; number: number; query: URLSearchParams }) {
  const pid = detail.project.id
  const d = usePoll(() => api.reviewByNumber(pid, number), 4000, [pid, number])
  const tab = query.get('tab') ?? 'conversation'
  const [comment, setComment] = useState('')
  const [reply, setReply] = useState<Record<number, string>>({})
  const a = useAction()
  const now = useNow(30000)
  if (d.error && !d.data) return <p className="error">{d.error}</p>
  if (!d.data) return <p className="muted">Loading…</p>
  const { review: r, status, files, diff, commits, comments, events, change_set } = d.data
  const canWrite = detail.role !== 'read'
  const act = (f: () => Promise<unknown>) =>
    a.run(async () => {
      await f()
      d.reload()
    })
  const replies = (c: ReviewComment) => comments.filter((x) => x.parent_id === c.id)

  const thread = (c: ReviewComment) => (
    <div key={c.id} className={`thread ${c.resolved ? 'resolved' : ''}`}>
      {[c, ...replies(c)].map((x) => (
        <div key={x.id} className="comment">
          <span className="muted">
            {x.author_kind === 'bot' ? '🤖 ' : ''}
            {x.author} · {ago(x.created_at, now)}
            {x.sha && x.sha !== r.head_sha && ' · outdated'}
          </span>
          <Text body={x.body} pid={pid} />
        </div>
      ))}
      {canWrite && (
        <div className="inline">
          <input
            className="grow"
            placeholder="Reply"
            value={reply[c.id] ?? ''}
            onChange={(e) => setReply({ ...reply, [c.id]: e.target.value })}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && reply[c.id]?.trim())
                act(async () => {
                  await api.commentReview(r.id, { body: reply[c.id], parent_id: c.id })
                  setReply({ ...reply, [c.id]: '' })
                })
            }}
          />
          <button className="secondary small" onClick={() => act(() => api.resolveComment(r.id, c.id, !c.resolved))}>
            {c.resolved ? 'Unresolve' : 'Resolve'}
          </button>
        </div>
      )}
    </div>
  )

  const base = `#/p/${pid}/reviews/${number}`
  return (
    <>
      <section className="run-head">
        <a href={`#/p/${pid}/reviews`} className="muted">
          ← reviews
        </a>
        <h1>
          {r.title} <span className="muted">!{r.number}</span> <Badge status={r.state} />
        </h1>
        <p className="muted">
          {r.author_kind === 'bot' ? '🤖 ' : ''}
          {r.author} wants to merge <code>{r.source_branch}</code> into <code>{r.target_branch}</code> in {r.repo}
          {r.issue_key && (
            <>
              {' '}
              for{' '}
              <a className="link" href={`#/p/${pid}/issues/${r.issue_key.split('-').pop()}`}>
                {r.issue_key}
              </a>
            </>
          )}
          {r.merge_sha && (
            <>
              {' '}
              · merged as <code className="sha">{r.merge_sha.slice(0, 10)}</code>
            </>
          )}
        </p>
      </section>

      {status && (
        <section className={`panel merge-box ${status.mergeable ? 'ok' : ''}`}>
          <div className="checks">
            <strong>Checks:</strong> <Badge status={status.checks.state} />
            {status.checks.run && (
              <a className="link" href={`#/runs/${status.checks.run.id}`}>
                {' '}
                run #{status.checks.run.id}
              </a>
            )}
            {status.checks.required?.map((j) => (
              <span key={j.name} className="label">
                {j.name}: {j.status}
              </span>
            ))}
          </div>
          <div>
            <strong>Approvals:</strong>{' '}
            {status.approvals.length === 0 && <span className="muted">none yet ({status.rules.required_approvals} needed)</span>}
            {status.approvals.map((x) => (
              <span key={x.user} className={`label ${x.state} ${x.stale ? 'stale' : ''}`}>
                {x.kind === 'bot' ? '🤖 ' : ''}
                {x.user}: {x.state.replace('_', ' ')}
                {x.stale ? ' (older commit)' : ''}
              </span>
            ))}
          </div>
          {status.blockers.length > 0 ? (
            <ul className="blockers">
              {status.blockers.map((b) => (
                <li key={b}>{b}</li>
              ))}
            </ul>
          ) : (
            <p className="ok-text">Ready to merge with {status.rules.merge}.</p>
          )}
          {r.queue_state && (
            <p>
              In the merge queue: <Badge status={r.queue_state} />
              {r.queue_run_id && (
                <a className="link" href={`#/runs/${r.queue_run_id}`}>
                  {' '}
                  run #{r.queue_run_id}
                </a>
              )}
            </p>
          )}
          {canWrite && (
            <div className="inline">
              {r.state === 'draft' ? (
                <button onClick={() => act(() => api.updateReview(r.id, { state: 'open' }))}>Ready for review</button>
              ) : (
                <>
                  <button className="secondary" onClick={() => act(() => api.approve(r.id, 'approved', comment || undefined).then(() => setComment('')))}>
                    Approve
                  </button>
                  <button className="secondary" onClick={() => act(() => api.approve(r.id, 'changes_requested', comment || undefined).then(() => setComment('')))}>
                    Request changes
                  </button>
                  {r.queue_state ? (
                    <button className="secondary" onClick={() => act(() => api.dequeue(r.id))}>
                      Remove from queue
                    </button>
                  ) : (
                    <button disabled={!status.mergeable && !status.rules.merge_queue} onClick={() => act(() => api.merge(r.id))}>
                      {status.rules.merge_queue ? 'Add to merge queue' : 'Merge'}
                    </button>
                  )}
                </>
              )}
              <button className="danger" onClick={() => act(() => api.updateReview(r.id, { state: 'closed' }))}>
                Close
              </button>
            </div>
          )}
        </section>
      )}
      {r.state === 'closed' && canWrite && (
        <button className="secondary" onClick={() => act(() => api.updateReview(r.id, { state: 'open' }))}>
          Reopen
        </button>
      )}
      <Err text={a.error} />

      {change_set.length > 0 && (
        <section className="panel">
          <strong>Change set</strong> <span className="muted">(merged together, all repos at once)</span>
          {change_set.map((x) => (
            <a key={x.id} className="ref-row" href={`#/p/${pid}/reviews/${x.number}`}>
              <Badge status={x.state} /> {x.repo} !{x.number} {x.title}
            </a>
          ))}
        </section>
      )}

      <nav className="tabs">
        <a href={base} className={tab === 'conversation' ? 'active' : ''}>
          Conversation
        </a>
        <a href={`${base}?tab=commits`} className={tab === 'commits' ? 'active' : ''}>
          Commits <span className="muted">{commits.length}</span>
        </a>
        <a href={`${base}?tab=files`} className={tab === 'files' ? 'active' : ''}>
          Files <span className="muted">{files.length}</span>
        </a>
      </nav>

      {tab === 'conversation' && (
        <>
          {r.body && <Text body={r.body} pid={pid} />}
          <div className="timeline">
            {[
              ...events.map((e) => ({ at: e.created_at, el: (
                <div key={`e${e.created_at}${e.kind}`} className="event">
                  <span className="muted">
                    {e.actor ?? 'moloko'} · {e.kind.replace('_', ' ')} · {ago(e.created_at, now)}
                  </span>
                  {e.body && <Text body={e.body} pid={pid} />}
                </div>
              ) })),
              ...comments
                .filter((c) => c.parent_id == null)
                .map((c) => ({ at: c.created_at, el: (
                  <div key={`c${c.id}`}>
                    {c.path && (
                      <div className="muted small-text">
                        {c.path}:{c.line}
                      </div>
                    )}
                    {thread(c)}
                  </div>
                ) })),
            ]
              .sort((x, y) => x.at - y.at)
              .map((x) => x.el)}
          </div>
          {canWrite && (
            <form
              className="form"
              onSubmit={(e) => {
                e.preventDefault()
                act(async () => {
                  await api.commentReview(r.id, { body: comment })
                  setComment('')
                })
              }}
            >
              <textarea value={comment} onChange={(e) => setComment(e.target.value)} placeholder="Comment. Approve or request changes above to include it with your review." />
              <div className="inline">
                <button type="submit" disabled={!comment.trim()}>
                  Comment
                </button>
              </div>
            </form>
          )}
        </>
      )}

      {tab === 'commits' && (
        <table className="runs">
          <tbody>
            {commits.map((c) => (
              <tr key={c.sha} onClick={() => go(`#/p/${pid}/code/${r.repo_id}?view=commit&sha=${c.sha}`)}>
                <td>
                  <code className="sha">{c.sha.slice(0, 7)}</code>
                </td>
                <td>{c.subject}</td>
                <td className="muted">{c.author}</td>
                <td className="muted right">{ago(c.date, now)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {tab === 'files' && (
        <>
          <p className="muted small-text">
            {files.length} files · +{files.reduce((n, f) => n + (f.added ?? 0), 0)} −{files.reduce((n, f) => n + (f.removed ?? 0), 0)}
            {canWrite && ' · click a line number to comment'}
          </p>
          <Diff
            diff={diff}
            comments={comments}
            renderThread={thread}
            onComment={canWrite ? (path, line, side, body) => act(() => api.commentReview(r.id, { body, path, line, side })) : undefined}
          />
        </>
      )}
    </>
  )
}
