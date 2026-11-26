import { useState } from 'react'
import { ago, api, type Issue, type IssueStatus, PRIORITIES, type ProjectDetail, type MirrorMode, STATUSES, TYPES, type User } from './api'
import { go, useAction, useNow, usePoll } from './hooks'
import { Badge, Err, Tabs, Text } from './ui'

export function Login({ setup, onDone }: { setup: boolean; onDone: (u: User) => void }) {
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const a = useAction()
  const submit = (e: React.FormEvent) => {
    e.preventDefault()
    a.run(async () => onDone((await (setup ? api.setup : api.login)(username, password)).user))
  }
  return (
    <form className="login" onSubmit={submit}>
      <h1>moloko</h1>
      <p className="muted">{setup ? 'Create the first admin account.' : 'Sign in to continue.'}</p>
      <input placeholder="username" autoFocus value={username} onChange={(e) => setUsername(e.target.value)} required />
      <input placeholder="password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} required minLength={setup ? 8 : 1} />
      <button type="submit" disabled={a.busy}>
        {setup ? 'Create admin' : 'Sign in'}
      </button>
      <Err text={a.error} />
    </form>
  )
}

export function ProjectsPage() {
  const projects = usePoll(api.projects, 30000, [])
  const [adding, setAdding] = useState(false)
  return (
    <>
      <section className="toolbar">
        <h1 className="page-title">Projects</h1>
        {!adding && <button onClick={() => setAdding(true)}>+ Project</button>}
      </section>
      {adding && <NewProject onClose={() => setAdding(false)} />}
      <Err text={projects.error} />
      {projects.data?.length === 0 && !adding && (
        <div className="empty">
          <p>No projects yet.</p>
          <p className="muted">A project holds issues, reviews, pipelines and one or more git repositories, hosted here or mirrored with GitHub.</p>
        </div>
      )}
      <div className="cards">
        {projects.data?.map((p) => (
          <a key={p.id} className="card" href={`#/p/${p.id}`}>
            <strong>{p.name}</strong> <span className="label">{p.key}</span>
            {p.repo_url && <div className="muted small-text">{p.repo_url}</div>}
          </a>
        ))}
      </div>
    </>
  )
}

const MODES: [MirrorMode, string][] = [
  ['none', 'Hosted here only'],
  ['pull', 'Pull mirror (read-only, GitHub is the source)'],
  ['push', 'Push mirror (moloko is the source)'],
  ['twoway', 'Two-way mirror'],
]

function NewProject({ onClose }: { onClose: () => void }) {
  const [name, setName] = useState('')
  const [key, setKey] = useState('')
  const [url, setUrl] = useState('')
  const [mode, setMode] = useState<MirrorMode>('pull')
  const [credential, setCredential] = useState('')
  const a = useAction()
  const submit = (e: React.FormEvent) => {
    e.preventDefault()
    a.run(async () => {
      const { id } = await api.createProject({
        name,
        key: key || undefined,
        repo_url: url || undefined,
        mirror_mode: url ? mode : undefined,
        credential: credential || undefined,
      })
      go(`#/p/${id}`)
    })
  }
  return (
    <form className="panel form" onSubmit={submit}>
      <label>
        Name <input value={name} onChange={(e) => setName(e.target.value)} required autoFocus />
      </label>
      <label>
        Issue key <input value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} placeholder="from the name, e.g. SHOP" size={8} />
      </label>
      <label>
        GitHub URL <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="optional: https://github.com/org/repo.git" size={44} />
      </label>
      {url && (
        <>
          <label>
            Mirroring
            <select value={mode} onChange={(e) => setMode(e.target.value as MirrorMode)}>
              {MODES.filter(([m]) => m !== 'none').map(([m, l]) => (
                <option key={m} value={m}>
                  {l}
                </option>
              ))}
            </select>
          </label>
          <label>
            Token <input type="password" value={credential} onChange={(e) => setCredential(e.target.value)} placeholder="needed for private repos and for pushing" size={30} />
          </label>
        </>
      )}
      <p className="muted small-text">Without a URL the project starts with an empty hosted repository. You can add more repositories in Settings.</p>
      <div className="inline">
        <button type="submit" disabled={a.busy}>
          Create
        </button>
        <button type="button" className="secondary" onClick={onClose}>
          Cancel
        </button>
        <Err text={a.error} />
      </div>
    </form>
  )
}

export { MODES }

/** Project header and tabs; the tab body comes from App. */
export function ProjectShell({ detail, tab, children }: { detail: ProjectDetail; tab: string; children: React.ReactNode }) {
  const p = detail.project
  const base = `#/p/${p.id}`
  return (
    <>
      <section className="project-head">
        <h1>
          {p.name} <span className="label">{p.key}</span>
        </h1>
        <Tabs
          current={tab}
          tabs={[
            [`${base}`, 'Issues'],
            [`${base}/board`, 'Board'],
            [`${base}/reviews`, 'Reviews'],
            [`${base}/code`, 'Code'],
            [`${base}/runs`, 'Runs'],
            [`${base}/ai`, 'AI agents'],
            [`${base}/settings`, 'Settings'],
          ]}
        />
      </section>
      {children}
    </>
  )
}

function IssueRow({ i, pid, now }: { i: Issue; pid: number; now: number }) {
  return (
    <tr onClick={() => go(`#/p/${pid}/issues/${i.number}`)}>
      <td className="num">{i.key}</td>
      <td>
        <span className={`prio ${i.priority}`} title={`priority ${i.priority}`} />
        <span className="type">{i.type}</span> <strong>{i.title}</strong>
        {i.labels.map((l) => (
          <span key={l} className="label">
            {l}
          </span>
        ))}
      </td>
      <td>
        <Badge status={i.status} />
      </td>
      <td className="muted">{i.assignee ? `${i.assignee_kind === 'bot' ? '🤖 ' : ''}@${i.assignee}` : ''}</td>
      <td className="muted right">{ago(i.updated_at, now)}</td>
    </tr>
  )
}

export function IssuesTab({ detail, query }: { detail: ProjectDetail; query: URLSearchParams }) {
  const pid = detail.project.id
  const [text, setText] = useState(query.get('q') ?? 'status:open')
  const [search, setSearch] = useState(text)
  const issues = usePoll(() => api.issues(pid, search), 10000, [pid, search])
  const [adding, setAdding] = useState(false)
  const now = useNow(30000)
  return (
    <>
      <section className="toolbar">
        <form
          className="inline grow"
          onSubmit={(e) => {
            e.preventDefault()
            setSearch(text)
          }}
        >
          <input className="grow" value={text} onChange={(e) => setText(e.target.value)} placeholder="status:open assignee:me label:web type:bug priority:high text…" />
          <button type="submit" className="secondary">
            Search
          </button>
        </form>
        {detail.role !== 'read' && !adding && <button onClick={() => setAdding(true)}>+ Issue</button>}
      </section>
      {adding && (
        <NewIssue
          detail={detail}
          onClose={(n) => {
            setAdding(false)
            if (n) go(`#/p/${pid}/issues/${n}`)
          }}
        />
      )}
      <Err text={issues.error} />
      {issues.data?.length === 0 && <p className="muted">No issues match.</p>}
      <table className="runs">
        <tbody>
          {issues.data?.map((i) => (
            <IssueRow key={i.id} i={i} pid={pid} now={now} />
          ))}
        </tbody>
      </table>
    </>
  )
}

function NewIssue({ detail, onClose, status }: { detail: ProjectDetail; onClose: (n?: number) => void; status?: IssueStatus }) {
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  const [type, setType] = useState<Issue['type']>('task')
  const [assignee, setAssignee] = useState('')
  const a = useAction()
  const submit = (e: React.FormEvent) => {
    e.preventDefault()
    a.run(async () => {
      const i = await api.createIssue(detail.project.id, { title, body, type, assignee: assignee || undefined, status })
      onClose(i.number)
    })
  }
  return (
    <form className="panel form" onSubmit={submit}>
      <input placeholder="Title" value={title} onChange={(e) => setTitle(e.target.value)} required autoFocus className="wide" />
      <textarea placeholder="Description" value={body} onChange={(e) => setBody(e.target.value)} />
      <div className="inline">
        <select value={type} onChange={(e) => setType(e.target.value as Issue['type'])}>
          {TYPES.map((t) => (
            <option key={t}>{t}</option>
          ))}
        </select>
        <AssigneeSelect detail={detail} value={assignee} onChange={setAssignee} />
        <button type="submit" disabled={a.busy}>
          Create
        </button>
        <button type="button" className="secondary" onClick={() => onClose()}>
          Cancel
        </button>
        <Err text={a.error} />
      </div>
    </form>
  )
}

function AssigneeSelect({ detail, value, onChange }: { detail: ProjectDetail; value: string; onChange: (v: string) => void }) {
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}>
      <option value="">Unassigned</option>
      {detail.members.map(({ user }) => (
        <option key={user.id} value={user.username}>
          {user.kind === 'bot' ? '🤖 ' : ''}
          {user.username}
        </option>
      ))}
    </select>
  )
}

const OPEN: IssueStatus[] = ['backlog', 'todo', 'in_progress', 'in_review', 'done']

export function BoardTab({ detail }: { detail: ProjectDetail }) {
  const pid = detail.project.id
  const [filter, setFilter] = useState('')
  const issues = usePoll(() => api.issues(pid, `${filter} status:${OPEN.join(',')}`), 5000, [pid, filter])
  const [dragging, setDragging] = useState<Issue | null>(null)
  const [over, setOver] = useState<IssueStatus | null>(null)
  const [adding, setAdding] = useState<IssueStatus | null>(null)
  const a = useAction()
  const canEdit = detail.role !== 'read'
  const drop = (status: IssueStatus, before?: Issue) => {
    const i = dragging
    setDragging(null)
    setOver(null)
    if (!i || !issues.data) return
    const col = issues.data.filter((x) => x.status === status && x.id !== i.id)
    const idx = before ? col.findIndex((x) => x.id === before.id) : col.length
    const prev = col[idx - 1]?.rank
    const next = col[idx]?.rank
    const rank = prev === undefined && next === undefined ? i.rank : prev === undefined ? next! - 1 : next === undefined ? prev + 1 : (prev + next) / 2
    a.run(async () => {
      await api.updateIssue(i.id, i.status === status ? { rank } : { status, rank })
      issues.reload()
    })
  }
  return (
    <>
      <section className="toolbar">
        <input className="grow" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="filter: assignee:me label:web type:bug" />
        <Err text={a.error ?? issues.error} />
      </section>
      {adding && (
        <NewIssue
          detail={detail}
          status={adding}
          onClose={() => {
            setAdding(null)
            issues.reload()
          }}
        />
      )}
      <div className="board">
        {OPEN.map((st) => {
          const col = (issues.data ?? []).filter((i) => i.status === st)
          return (
            <div
              key={st}
              className={`column ${over === st ? 'over' : ''}`}
              onDragOver={(e) => {
                e.preventDefault()
                setOver(st)
              }}
              onDragLeave={() => setOver(null)}
              onDrop={() => drop(st)}
            >
              <div className="column-head">
                <Badge status={st} /> <span className="muted">{col.length}</span>
                {canEdit && (
                  <button className="secondary small" onClick={() => setAdding(st)}>
                    +
                  </button>
                )}
              </div>
              {col.map((i) => (
                <a
                  key={i.id}
                  href={`#/p/${pid}/issues/${i.number}`}
                  className={`issue-card ${dragging?.id === i.id ? 'dragging' : ''}`}
                  draggable={canEdit}
                  onDragStart={() => setDragging(i)}
                  onDragEnd={() => setDragging(null)}
                  onDrop={(e) => {
                    e.stopPropagation()
                    drop(st, i)
                  }}
                >
                  <div className="muted small-text">
                    <span className={`prio ${i.priority}`} />
                    {i.key} · {i.type}
                  </div>
                  <div>{i.title}</div>
                  {i.assignee && (
                    <div className="muted small-text">
                      {i.assignee_kind === 'bot' ? '🤖 ' : ''}@{i.assignee}
                    </div>
                  )}
                </a>
              ))}
            </div>
          )
        })}
      </div>
    </>
  )
}

export function IssuePage({ detail, number }: { detail: ProjectDetail; number: number }) {
  const pid = detail.project.id
  const d = usePoll(() => api.issueByNumber(pid, number), 4000, [pid, number])
  const agents = usePoll(() => api.aiAgents(pid), 60000, [pid])
  const cycles = usePoll(() => api.cycles(pid), 60000, [pid])
  const [editing, setEditing] = useState(false)
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  const [comment, setComment] = useState('')
  const [link, setLink] = useState({ to: '', kind: 'relates' })
  const [ai, setAi] = useState({ agent: '', instruction: '' })
  const a = useAction()
  const now = useNow(30000)
  if (d.error && !d.data) return <p className="error">{d.error}</p>
  if (!d.data) return <p className="muted">Loading…</p>
  const { issue, events, refs, links, children, parent, reviews, sessions, branch_name } = d.data
  const canEdit = detail.role !== 'read'
  const set = (field: string, value: unknown) =>
    a.run(async () => {
      await api.updateIssue(issue.id, { [field]: value })
      d.reload()
    })
  return (
    <div className="issue-page">
      <div className="main-col">
        <a href={`#/p/${pid}`} className="muted">
          ← issues
        </a>
        {editing ? (
          <form
            className="form"
            onSubmit={(e) => {
              e.preventDefault()
              a.run(async () => {
                await api.updateIssue(issue.id, { title, body })
                setEditing(false)
                d.reload()
              })
            }}
          >
            <input className="wide" value={title} onChange={(e) => setTitle(e.target.value)} required />
            <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={10} />
            <div className="inline">
              <button type="submit">Save</button>
              <button type="button" className="secondary" onClick={() => setEditing(false)}>
                Cancel
              </button>
            </div>
          </form>
        ) : (
          <>
            <h1>
              <span className="muted">{issue.key}</span> {issue.title}
              {canEdit && (
                <button
                  className="secondary small"
                  onClick={() => {
                    setTitle(issue.title)
                    setBody(issue.body)
                    setEditing(true)
                  }}
                >
                  Edit
                </button>
              )}
            </h1>
            {parent && (
              <p className="muted">
                Part of{' '}
                <a className="link" href={`#/p/${pid}/issues/${parent.number}`}>
                  {parent.key} {parent.title}
                </a>
              </p>
            )}
            {issue.body ? <Text body={issue.body} pid={pid} /> : <p className="muted">No description.</p>}
          </>
        )}
        <Err text={a.error} />

        {children.length > 0 && (
          <section>
            <h2>Sub-issues</h2>
            <table className="runs">
              <tbody>
                {children.map((c) => (
                  <IssueRow key={c.id} i={c} pid={pid} now={now} />
                ))}
              </tbody>
            </table>
          </section>
        )}

        {reviews.length > 0 && (
          <section>
            <h2>Reviews</h2>
            {reviews.map((r) => (
              <a key={r.id} className="ref-row" href={`#/p/${pid}/reviews/${r.number}`}>
                <Badge status={r.state} /> !{r.number} {r.title} <span className="muted">{r.repo}</span>
              </a>
            ))}
          </section>
        )}

        {sessions.length > 0 && (
          <section>
            <h2>AI sessions</h2>
            {sessions.map((s) => (
              <div key={s.id} className="ref-row">
                <Badge status={s.state} /> 🤖 {s.agent} <span className="muted">{s.kind}</span>
                {s.run_id && (
                  <a className="link" href={`#/runs/${s.run_id}`}>
                    {' '}
                    logs
                  </a>
                )}
                {s.result && <span className="muted"> · {s.result}</span>}
                <span className="muted right-float">{ago(s.created_at, now)}</span>
              </div>
            ))}
          </section>
        )}

        <section>
          <h2>Activity</h2>
          <div className="timeline">
            {events.map((e) => (
              <div key={e.id} className={`event ev-${e.kind}`}>
                <span className="muted">
                  {e.actor_kind === 'bot' ? '🤖 ' : ''}
                  {e.actor ?? 'moloko'} · {e.kind === 'field' ? fieldChange(e.data, detail) : e.kind.replace('_', ' ')} · {ago(e.created_at, now)}
                </span>
                {e.body && <Text body={e.body} pid={pid} />}
              </div>
            ))}
          </div>
          {canEdit && (
            <form
              className="form"
              onSubmit={(e) => {
                e.preventDefault()
                a.run(async () => {
                  await api.commentIssue(issue.id, comment)
                  setComment('')
                  d.reload()
                })
              }}
            >
              <textarea value={comment} onChange={(e) => setComment(e.target.value)} placeholder="Comment. Mention an AI agent with @name to hand it work." />
              <div className="inline">
                <button type="submit" disabled={!comment.trim()}>
                  Comment
                </button>
              </div>
            </form>
          )}
        </section>
      </div>

      <aside className="side-col">
        <Field label="Status">
          <select value={issue.status} disabled={!canEdit} onChange={(e) => set('status', e.target.value)}>
            {STATUSES.map((s) => (
              <option key={s} value={s}>
                {s.replace('_', ' ')}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Assignee">
          <AssigneeSelect detail={detail} value={issue.assignee ?? ''} onChange={(v) => set('assignee', v)} />
        </Field>
        <Field label="Type">
          <select value={issue.type} disabled={!canEdit} onChange={(e) => set('type', e.target.value)}>
            {TYPES.map((t) => (
              <option key={t}>{t}</option>
            ))}
          </select>
        </Field>
        <Field label="Priority">
          <select value={issue.priority} disabled={!canEdit} onChange={(e) => set('priority', e.target.value)}>
            {PRIORITIES.map((t) => (
              <option key={t}>{t}</option>
            ))}
          </select>
        </Field>
        <Field label="Cycle">
          <select value={issue.cycle_id ?? 0} disabled={!canEdit} onChange={(e) => set('cycle_id', Number(e.target.value))}>
            <option value={0}>None</option>
            {cycles.data?.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Estimate">
          <input type="number" min={0} disabled={!canEdit} defaultValue={issue.estimate ?? ''} onBlur={(e) => e.target.value && set('estimate', Number(e.target.value))} />
        </Field>
        <Field label="Labels">
          <input
            disabled={!canEdit}
            defaultValue={issue.labels.join(', ')}
            onBlur={(e) => {
              const v = e.target.value.split(',').map((l) => l.trim()).filter(Boolean)
              if (v.join() !== issue.labels.join()) set('labels', v)
            }}
          />
        </Field>
        <Field label="Parent">
          <input
            disabled={!canEdit}
            placeholder="issue number"
            defaultValue={parent?.number ?? ''}
            onBlur={async (e) => {
              const n = Number(e.target.value.replace(/\D/g, ''))
              if ((n || 0) === (parent?.number ?? 0)) return
              if (!n) return set('parent_id', 0)
              a.run(async () => {
                const p = await api.issueByNumber(pid, n)
                await api.updateIssue(issue.id, { parent_id: p.issue.id })
                d.reload()
              })
            }}
          />
        </Field>
        {issue.released_env && (
          <Field label="Released">
            <span>
              {issue.released_env} · {ago(issue.released_at!, now)}
            </span>
          </Field>
        )}

        <h3>Branch</h3>
        <code className="pull">{branch_name}</code>
        {canEdit &&
          detail.repos
            .filter((r) => r.mirror_mode !== 'pull')
            .map((r) => (
              <button
                key={r.id}
                className="secondary small block"
                onClick={() =>
                  a.run(async () => {
                    await api.issueBranch(issue.id, r.id)
                    d.reload()
                  })
                }
              >
                Create in {r.name}
              </button>
            ))}

        {refs.length > 0 && (
          <>
            <h3>Development</h3>
            {refs.map((r, i) => (
              <div key={i} className="small-text">
                <span className="label">{r.kind}</span> {r.repo && <span className="muted">{r.repo} </span>}
                {r.kind === 'run' ? (
                  <a className="link" href={`#/runs/${r.ref}`}>
                    run #{r.ref}
                  </a>
                ) : r.kind === 'review' ? (
                  <a className="link" href={`#/p/${pid}/reviews/${r.ref.replace('!', '')}`}>
                    {r.ref}
                  </a>
                ) : r.kind === 'commit' && r.repo_id ? (
                  <a className="link" href={`#/p/${pid}/code/${r.repo_id}?view=commit&sha=${r.sha}`}>
                    {r.sha?.slice(0, 7)}
                  </a>
                ) : (
                  <span>{r.ref.replace(`${r.repo}:`, '')}</span>
                )}
              </div>
            ))}
          </>
        )}

        <h3>Links</h3>
        {links.map((l) => (
          <div key={`${l.kind}${l.id}${l.direction}`} className="small-text">
            <span className="muted">{l.direction === 'out' ? l.kind : `${l.kind} (from)`}</span>{' '}
            <a className="link" href={`#/p/${pid}/issues/${l.key.split('-').pop()}`}>
              {l.key}
            </a>{' '}
            {l.title} <Badge status={l.status} />
          </div>
        ))}
        {canEdit && (
          <form
            className="inline"
            onSubmit={(e) => {
              e.preventDefault()
              a.run(async () => {
                await api.linkIssue(issue.id, link.to, link.kind)
                setLink({ ...link, to: '' })
                d.reload()
              })
            }}
          >
            <select value={link.kind} onChange={(e) => setLink({ ...link, kind: e.target.value })}>
              <option>relates</option>
              <option>blocks</option>
              <option>duplicates</option>
            </select>
            <input size={8} placeholder={`${detail.project.key}-1`} value={link.to} onChange={(e) => setLink({ ...link, to: e.target.value })} />
            <button className="secondary small" type="submit" disabled={!link.to}>
              Link
            </button>
          </form>
        )}

        {canEdit && (agents.data?.length ?? 0) > 0 && (
          <>
            <h3>Hand to an AI agent</h3>
            <form
              className="form"
              onSubmit={(e) => {
                e.preventDefault()
                a.run(async () => {
                  await api.startAi(issue.id, ai.agent || agents.data![0].name, ai.instruction)
                  setAi({ ...ai, instruction: '' })
                  d.reload()
                })
              }}
            >
              <select value={ai.agent} onChange={(e) => setAi({ ...ai, agent: e.target.value })}>
                {agents.data!.map((g) => (
                  <option key={g.id} value={g.name}>
                    🤖 {g.name} ({g.environment.kind})
                  </option>
                ))}
              </select>
              <textarea rows={3} placeholder="Extra instructions (optional)" value={ai.instruction} onChange={(e) => setAi({ ...ai, instruction: e.target.value })} />
              <button type="submit">Start session</button>
            </form>
          </>
        )}
      </aside>
    </div>
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="field">
      <span className="muted">{label}</span>
      {children}
    </label>
  )
}

function fieldChange(data: Record<string, unknown> | null, detail: ProjectDetail) {
  if (!data) return 'changed'
  const name = String(data.field).replace(/_id$/, '')
  const show = (v: unknown) => {
    if (v == null || v === '') return 'none'
    if (name === 'assignee') return detail.members.find((m) => m.user.id === v)?.user.username ?? `#${v}`
    return Array.isArray(v) ? v.join(', ') : String(v).replace('_', ' ')
  }
  return data.from == null ? `set ${name} to ${show(data.to)}` : `changed ${name} from ${show(data.from)} to ${show(data.to)}`
}
