import { useState } from 'react'
import { ago, api, type AiAgent, type MirrorMode, type ProjectDetail, type Repo, type Role, type User } from './api'
import { MODES } from './forge'
import { useAction, useNow, usePoll } from './hooks'
import { Badge, Err } from './ui'

export function SettingsTab({ detail, reload }: { detail: ProjectDetail; reload: () => void }) {
  const admin = detail.role === 'admin'
  return (
    <>
      <Repos detail={detail} reload={reload} admin={admin} />
      <Members detail={detail} reload={reload} admin={admin} />
      <Cycles pid={detail.project.id} canEdit={detail.role !== 'read'} />
      {admin && <ProjectKey detail={detail} reload={reload} />}
      {admin && <Secrets pid={detail.project.id} />}
      {detail.role !== 'read' && <Import pid={detail.project.id} />}
    </>
  )
}

function ProjectKey({ detail, reload }: { detail: ProjectDetail; reload: () => void }) {
  const [key, setKey] = useState(detail.project.key)
  const a = useAction()
  return (
    <section className="settings">
      <h2>Issue key</h2>
      <form
        className="inline"
        onSubmit={(e) => {
          e.preventDefault()
          a.run(async () => {
            await api.updateProject(detail.project.id, { key })
            reload()
          })
        }}
      >
        <input value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} size={8} />
        <button type="submit" className="secondary">
          Save
        </button>
        <Err text={a.error} />
      </form>
    </section>
  )
}

function Repos({ detail, reload, admin }: { detail: ProjectDetail; reload: () => void; admin: boolean }) {
  const [adding, setAdding] = useState(false)
  const [form, setForm] = useState({ name: '', upstream_url: '', mirror_mode: 'none' as MirrorMode, credential: '' })
  const a = useAction()
  return (
    <section className="settings">
      <h2>Repositories</h2>
      {detail.repos.map((r) => (
        <RepoSettings key={r.id} repo={r} admin={admin} reload={reload} />
      ))}
      {admin &&
        (adding ? (
          <form
            className="panel form"
            onSubmit={(e) => {
              e.preventDefault()
              a.run(async () => {
                await api.createRepo(detail.project.id, {
                  name: form.name,
                  upstream_url: form.upstream_url || undefined,
                  mirror_mode: form.upstream_url ? form.mirror_mode : 'none',
                  credential: form.credential || undefined,
                })
                setAdding(false)
                reload()
              })
            }}
          >
            <div className="inline">
              <input placeholder="name, e.g. api" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} required />
              <input placeholder="GitHub URL (optional)" size={40} value={form.upstream_url} onChange={(e) => setForm({ ...form, upstream_url: e.target.value })} />
              {form.upstream_url && (
                <>
                  <select value={form.mirror_mode} onChange={(e) => setForm({ ...form, mirror_mode: e.target.value as MirrorMode })}>
                    {MODES.map(([m, l]) => (
                      <option key={m} value={m}>
                        {l}
                      </option>
                    ))}
                  </select>
                  <input type="password" placeholder="token" value={form.credential} onChange={(e) => setForm({ ...form, credential: e.target.value })} />
                </>
              )}
              <button type="submit">Add</button>
              <button type="button" className="secondary" onClick={() => setAdding(false)}>
                Cancel
              </button>
              <Err text={a.error} />
            </div>
          </form>
        ) : (
          <button className="secondary" onClick={() => setAdding(true)}>
            + Repository
          </button>
        ))}
    </section>
  )
}

function RepoSettings({ repo, admin, reload }: { repo: Repo; admin: boolean; reload: () => void }) {
  const [open, setOpen] = useState(false)
  const [f, setF] = useState({ ...repo, credential: '' })
  const mirror = usePoll(() => (repo.mirror_mode === 'none' ? Promise.resolve([]) : api.mirror(repo.id)), 15000, [repo.id, repo.mirror_mode])
  const a = useAction()
  const now = useNow(30000)
  const problems = (mirror.data ?? []).filter((m) => m.state !== 'in_sync')
  return (
    <div className="panel repo">
      <div className="inline">
        <strong>{repo.name}</strong>
        <span className="label">{MODES.find(([m]) => m === repo.mirror_mode)?.[1]}</span>
        {repo.upstream_url && <span className="muted small-text">{repo.upstream_url}</span>}
        <span className="grow" />
        {repo.mirror_mode !== 'none' && (
          <button
            className="secondary small"
            disabled={a.busy}
            onClick={() =>
              a.run(async () => {
                await api.syncRepo(repo.id)
                mirror.reload()
              })
            }
          >
            Sync now
          </button>
        )}
        {admin && (
          <button className="secondary small" onClick={() => setOpen(!open)}>
            {open ? 'Close' : 'Edit'}
          </button>
        )}
      </div>
      {problems.map((m) => (
        <div key={m.ref} className="mirror-row small-text">
          <span className={`label ${m.state}`}>{m.state}</span> <code>{m.ref}</code>{' '}
          <span className="muted">
            here {m.local_sha?.slice(0, 7) ?? '—'} · GitHub {m.remote_sha?.slice(0, 7) ?? '—'} · {ago(m.updated_at, now)}
          </span>
          {m.last_error && <div className="error">{m.last_error}</div>}
          {m.state === 'diverged' && admin && (
            <div className="inline">
              <span className="muted">Keep:</span>
              <button className="secondary small" onClick={() => a.run(async () => { await api.resolveMirror(repo.id, m.ref, 'moloko'); mirror.reload() })}>
                moloko's version
              </button>
              <button className="secondary small" onClick={() => a.run(async () => { await api.resolveMirror(repo.id, m.ref, 'upstream'); mirror.reload() })}>
                GitHub's version
              </button>
              <span className="muted">The other side's commits are kept under refs/moloko/diverged/.</span>
            </div>
          )}
        </div>
      ))}
      <Err text={a.error ?? mirror.error} />
      {open && (
        <form
          className="form"
          onSubmit={(e) => {
            e.preventDefault()
            a.run(async () => {
              await api.updateRepo(repo.id, {
                default_branch: f.default_branch,
                upstream_url: f.upstream_url,
                mirror_mode: f.mirror_mode,
                on_unreachable: f.on_unreachable,
                protect_default: f.protect_default,
                require_review: f.require_review,
                ...(f.credential ? { credential: f.credential } : {}),
              })
              setOpen(false)
              reload()
            })
          }}
        >
          <label>
            Default branch <input value={f.default_branch} onChange={(e) => setF({ ...f, default_branch: e.target.value })} />
          </label>
          <label>
            GitHub URL <input size={44} value={f.upstream_url ?? ''} onChange={(e) => setF({ ...f, upstream_url: e.target.value })} />
          </label>
          <label>
            Mirroring
            <select value={f.mirror_mode} onChange={(e) => setF({ ...f, mirror_mode: e.target.value as MirrorMode })}>
              {MODES.map(([m, l]) => (
                <option key={m} value={m}>
                  {l}
                </option>
              ))}
            </select>
          </label>
          {f.mirror_mode === 'twoway' && (
            <label>
              When GitHub is unreachable
              <select value={f.on_unreachable} onChange={(e) => setF({ ...f, on_unreachable: e.target.value as Repo['on_unreachable'] })}>
                <option value="reject">reject the push</option>
                <option value="queue">accept and retry later</option>
              </select>
            </label>
          )}
          <label>
            Token <input type="password" placeholder={repo.has_credential ? 'saved; type to replace' : 'none'} value={f.credential} onChange={(e) => setF({ ...f, credential: e.target.value })} />
          </label>
          <label className="check">
            <input type="checkbox" checked={f.protect_default} onChange={(e) => setF({ ...f, protect_default: e.target.checked })} /> Protect the default branch (no force
            pushes or deletes)
          </label>
          <label className="check">
            <input type="checkbox" checked={f.require_review} onChange={(e) => setF({ ...f, require_review: e.target.checked })} /> Changes to the default branch need a
            review
          </label>
          <div className="inline">
            <button type="submit">Save</button>
            <button
              type="button"
              className="danger"
              onClick={() => {
                if (confirm(`Delete ${repo.name} and its git data? This cannot be undone.`))
                  a.run(async () => {
                    await api.deleteRepo(repo.id)
                    reload()
                  })
              }}
            >
              Delete repository
            </button>
          </div>
        </form>
      )}
    </div>
  )
}

function Members({ detail, reload, admin }: { detail: ProjectDetail; reload: () => void; admin: boolean }) {
  const [username, setUsername] = useState('')
  const [role, setRole] = useState<Role>('write')
  const a = useAction()
  return (
    <section className="settings">
      <h2>Members</h2>
      <table className="runs">
        <tbody>
          {detail.members.map(({ user, role }) => (
            <tr key={user.id} className="static">
              <td>
                {user.kind === 'bot' ? '🤖 ' : ''}
                <strong>{user.username}</strong>
              </td>
              <td>
                {admin ? (
                  <select value={role} onChange={(e) => a.run(async () => { await api.setMember(detail.project.id, user.username, e.target.value as Role); reload() })}>
                    <option>read</option>
                    <option>write</option>
                    <option>admin</option>
                  </select>
                ) : (
                  role
                )}
              </td>
              <td className="right">
                {admin && (
                  <button className="danger small" onClick={() => a.run(async () => { await api.removeMember(detail.project.id, user.id); reload() })}>
                    Remove
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {admin && (
        <form
          className="inline"
          onSubmit={(e) => {
            e.preventDefault()
            a.run(async () => {
              await api.setMember(detail.project.id, username, role)
              setUsername('')
              reload()
            })
          }}
        >
          <input placeholder="username" value={username} onChange={(e) => setUsername(e.target.value)} required />
          <select value={role} onChange={(e) => setRole(e.target.value as Role)}>
            <option>read</option>
            <option>write</option>
            <option>admin</option>
          </select>
          <button type="submit" className="secondary">
            Add member
          </button>
        </form>
      )}
      <Err text={a.error} />
    </section>
  )
}

function Cycles({ pid, canEdit }: { pid: number; canEdit: boolean }) {
  const cycles = usePoll(() => api.cycles(pid), 60000, [pid])
  const [f, setF] = useState({ name: '', start: '', end: '' })
  const a = useAction()
  return (
    <section className="settings">
      <h2>Cycles</h2>
      {cycles.data?.map((c) => (
        <Burndown key={c.id} id={c.id} name={c.name} pid={pid} />
      ))}
      {canEdit && (
        <form
          className="inline"
          onSubmit={(e) => {
            e.preventDefault()
            a.run(async () => {
              await api.createCycle(pid, f.name, new Date(f.start).getTime(), new Date(f.end).getTime())
              setF({ name: '', start: '', end: '' })
              cycles.reload()
            })
          }}
        >
          <input placeholder="Sprint 12" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} required />
          <input type="date" value={f.start} onChange={(e) => setF({ ...f, start: e.target.value })} required />
          <input type="date" value={f.end} onChange={(e) => setF({ ...f, end: e.target.value })} required />
          <button type="submit" className="secondary">
            Add cycle
          </button>
          <Err text={a.error} />
        </form>
      )}
    </section>
  )
}

function Burndown({ id, name, pid }: { id: number; name: string; pid: number }) {
  const b = usePoll(() => api.burndown(id), 60000, [id])
  if (!b.data) return null
  const { total, points, starts_at, ends_at } = b.data
  const w = 240
  const h = 50
  const span = Math.max(1, ends_at - starts_at)
  const x = (t: number) => ((t - starts_at) / span) * w
  const y = (v: number) => h - (total ? (v / total) * h : 0)
  const last = points[points.length - 1]?.remaining ?? total
  return (
    <div className="cycle">
      <a className="link" href={`#/p/${pid}?q=${encodeURIComponent(`cycle:${id}`)}`}>
        {name}
      </a>
      <span className="muted small-text">
        {' '}
        {new Date(starts_at).toLocaleDateString()} to {new Date(ends_at).toLocaleDateString()} · {last} of {total} open
      </span>
      <svg width={w} height={h} className="burndown" role="img" aria-label={`Burndown for ${name}`}>
        <line x1={0} y1={0} x2={w} y2={h} className="ideal" />
        <polyline points={points.map((p) => `${x(p.day)},${y(p.remaining)}`).join(' ')} className="actual" />
      </svg>
    </div>
  )
}

function Secrets({ pid }: { pid: number }) {
  const secrets = usePoll(() => api.secrets(pid), 60000, [pid])
  const [f, setF] = useState({ name: '', value: '' })
  const a = useAction()
  return (
    <section className="settings">
      <h2>Secrets</h2>
      <p className="muted small-text">Encrypted at rest. AI agents only see the secrets listed in their settings.</p>
      {secrets.data?.map((s) => (
        <div key={s} className="inline">
          <code>{s}</code>
          <button className="danger small" onClick={() => a.run(async () => { await api.deleteSecret(pid, s); secrets.reload() })}>
            Delete
          </button>
        </div>
      ))}
      <form
        className="inline"
        onSubmit={(e) => {
          e.preventDefault()
          a.run(async () => {
            await api.setSecret(pid, f.name, f.value)
            setF({ name: '', value: '' })
            secrets.reload()
          })
        }}
      >
        <input placeholder="ANTHROPIC_API_KEY" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value.toUpperCase() })} required />
        <input type="password" placeholder="value" value={f.value} onChange={(e) => setF({ ...f, value: e.target.value })} required />
        <button type="submit" className="secondary">
          Save secret
        </button>
        <Err text={a.error} />
      </form>
    </section>
  )
}

function Import({ pid }: { pid: number }) {
  const [format, setFormat] = useState<'jira-csv' | 'github-json'>('jira-csv')
  const [result, setResult] = useState<string | null>(null)
  const a = useAction()
  return (
    <section className="settings">
      <h2>Import issues</h2>
      <div className="inline">
        <select value={format} onChange={(e) => setFormat(e.target.value as typeof format)}>
          <option value="jira-csv">Jira CSV export</option>
          <option value="github-json">GitHub issues JSON (gh issue list --json)</option>
        </select>
        <input
          type="file"
          onChange={async (e) => {
            const file = e.target.files?.[0]
            if (!file) return
            const data = await file.text()
            a.run(async () => setResult(`Imported ${(await api.importIssues(pid, format, data)).created} issues.`))
          }}
        />
        {result && <span>{result}</span>}
        <Err text={a.error} />
      </div>
    </section>
  )
}

const BLANK: Partial<AiAgent> = {
  name: '',
  environment: { kind: 'container', image: '' },
  command: 'claude -p "$(cat "$MOLOKO_TASK_FILE")" --dangerously-skip-permissions',
  review_command: '',
  secrets: ['ANTHROPIC_API_KEY'],
  timeout_secs: 2700,
  max_sessions: 1,
  auto_fix_ci: false,
}

export function AiTab({ detail }: { detail: ProjectDetail }) {
  const pid = detail.project.id
  const agents = usePoll(() => api.aiAgents(pid), 30000, [pid])
  const sessions = usePoll(() => api.sessions(pid), 5000, [pid])
  const [editing, setEditing] = useState<Partial<AiAgent> | null>(null)
  const now = useNow(10000)
  const admin = detail.role === 'admin'
  return (
    <>
      <section className="toolbar">
        <p className="muted grow">
          AI agents are project members. Assign them an issue or mention them with @name and they work on the issue branch in an isolated container or VM,
          then open a review. They cannot merge.
        </p>
        {admin && !editing && <button onClick={() => setEditing(BLANK)}>+ AI agent</button>}
      </section>
      {editing && (
        <AgentForm
          pid={pid}
          agent={editing}
          onDone={() => {
            setEditing(null)
            agents.reload()
          }}
        />
      )}
      <Err text={agents.error} />
      <table className="runs">
        <tbody>
          {agents.data?.map((g) => (
            <tr key={g.id} className="static">
              <td>
                🤖 <strong>{g.name}</strong>
              </td>
              <td className="muted">
                {g.environment.kind}
                {g.environment.image && ` · ${g.environment.image}`}
                {g.environment.cpus ? ` · ${g.environment.cpus} cpu` : ''}
                {g.environment.memory ? ` · ${g.environment.memory}` : ''}
              </td>
              <td className="muted">
                {g.max_sessions} at a time{g.auto_fix_ci ? ' · fixes CI' : ''}
                {g.review_command ? ' · reviews' : ''}
              </td>
              <td className="right">
                {admin && (
                  <>
                    <button className="secondary small" onClick={() => setEditing(g)}>
                      Edit
                    </button>
                    <button
                      className="danger small"
                      onClick={async () => {
                        if (confirm(`Delete AI agent ${g.name}?`)) {
                          await api.deleteAiAgent(g.id).catch(() => {})
                          agents.reload()
                        }
                      }}
                    >
                      Delete
                    </button>
                  </>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <h2>Sessions</h2>
      {sessions.data?.length === 0 && <p className="muted">No sessions yet.</p>}
      <table className="runs">
        <tbody>
          {sessions.data?.map((s) => (
            <tr key={s.id} onClick={() => s.run_id && (location.hash = `#/runs/${s.run_id}`)}>
              <td className="num">#{s.id}</td>
              <td>
                <Badge status={s.state} />
              </td>
              <td>
                🤖 {s.agent} <span className="muted">· {s.kind}</span>
                {s.branch && <code className="sha">{s.branch}</code>}
                {s.result && <div className="muted small-text">{s.result}</div>}
              </td>
              <td className="muted right">{ago(s.created_at, now)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  )
}

function AgentForm({ pid, agent, onDone }: { pid: number; agent: Partial<AiAgent>; onDone: () => void }) {
  const [f, setF] = useState(agent)
  const env = f.environment!
  const setEnv = (e: Partial<AiAgent['environment']>) => setF({ ...f, environment: { ...env, ...e } })
  const a = useAction()
  return (
    <form
      className="panel form"
      onSubmit={(e) => {
        e.preventDefault()
        a.run(async () => {
          const body = { ...f, review_command: f.review_command || null }
          await (f.id ? api.updateAiAgent(f.id, body) : api.createAiAgent(pid, body))
          onDone()
        })
      }}
    >
      <label>
        Name <input value={f.name} disabled={!!f.id} onChange={(e) => setF({ ...f, name: e.target.value })} required placeholder="claude" />
      </label>
      <label>
        Environment
        <select value={env.kind} onChange={(e) => setEnv({ kind: e.target.value as typeof env.kind })}>
          <option value="container">Container (Docker)</option>
          <option value="vm">VM (tart, or a driver)</option>
          <option value="host">Host (no isolation)</option>
        </select>
      </label>
      {env.kind !== 'host' && (
        <label>
          {env.kind === 'vm' ? 'VM image' : 'Image'} <input size={40} value={env.image} onChange={(e) => setEnv({ image: e.target.value })} required placeholder="an image with your agent CLI and toolchain" />
        </label>
      )}
      {env.kind === 'vm' && (
        <label>
          Driver <input value={env.driver ?? ''} onChange={(e) => setEnv({ driver: e.target.value || null })} placeholder="tart, or a path to a driver program" />
        </label>
      )}
      {env.kind !== 'host' && (
        <div className="inline">
          <label>
            CPUs <input type="number" min={1} value={env.cpus ?? ''} onChange={(e) => setEnv({ cpus: e.target.value ? Number(e.target.value) : null })} />
          </label>
          <label>
            Memory <input value={env.memory ?? ''} onChange={(e) => setEnv({ memory: e.target.value || null })} placeholder="8g" size={6} />
          </label>
        </div>
      )}
      <label>
        Command <input className="wide" value={f.command} onChange={(e) => setF({ ...f, command: e.target.value })} required />
      </label>
      <label>
        Review command <input className="wide" value={f.review_command ?? ''} onChange={(e) => setF({ ...f, review_command: e.target.value })} placeholder="optional: runs when this agent is the AI reviewer" />
      </label>
      <label>
        Secrets <input value={(f.secrets ?? []).join(', ')} onChange={(e) => setF({ ...f, secrets: e.target.value.split(',').map((s) => s.trim()).filter(Boolean) })} />
      </label>
      <div className="inline">
        <label>
          Timeout (s) <input type="number" min={60} value={f.timeout_secs} onChange={(e) => setF({ ...f, timeout_secs: Number(e.target.value) })} />
        </label>
        <label>
          Parallel sessions <input type="number" min={1} max={32} value={f.max_sessions} onChange={(e) => setF({ ...f, max_sessions: Number(e.target.value) })} />
        </label>
        <label className="check">
          <input type="checkbox" checked={!!f.auto_fix_ci} onChange={(e) => setF({ ...f, auto_fix_ci: e.target.checked })} /> Fix failing CI on its own reviews
        </label>
      </div>
      <p className="muted small-text">
        The command runs in the checked-out issue branch with $MOLOKO_TASK_FILE (the instructions), $MOLOKO_BRANCH, $MOLOKO_DIFF_FILE and $MOLOKO_REVIEW_FILE for reviews,
        and a $MOLOKO_TOKEN that can only push this branch. Uncommitted changes are committed and pushed for you.
      </p>
      <div className="inline">
        <button type="submit">{f.id ? 'Save' : 'Create'}</button>
        <button type="button" className="secondary" onClick={onDone}>
          Cancel
        </button>
        <Err text={a.error} />
      </div>
    </form>
  )
}

/** moloko runners: subscription status and the minutes this server recorded itself this month. */
function CloudRunners() {
  const cloud = usePoll(api.cloud, 60000, [])
  const c = cloud.data
  if (!c) return null
  return (
    <section className="settings">
      <h2>Cloud runners</h2>
      {c.configured ? (
        <>
          <p className="muted small-text">
            Jobs with <code>runner: cloud</code> run on attested confidential VMs via {c.broker}. This server checks each runner's
            hardware attestation before it sends any source or secrets.
          </p>
          {c.insecure_dev && (
            <p className="error">Development mode: runners are not verified, so whoever runs them can read your jobs.</p>
          )}
          <div className="inline">
            <strong>{c.m_minutes.toFixed(0)} m-minutes</strong>
            <span className="muted small-text">this month, {c.jobs} cloud jobs, as recorded by this server</span>
          </div>
          <p className="muted small-text">
            Accepted runner images:{' '}
            {c.measurements?.length ? c.measurements.map((m) => <code key={m}>{m.slice(0, 16)}…</code>) : 'none (set --cloud-measurements)'}
          </p>
        </>
      ) : (
        <p className="muted small-text">
          Not subscribed. Start the server with <code>--cloud-key</code> and <code>--cloud-broker</code> to run jobs marked{' '}
          <code>runner: cloud</code> on moloko runners.
        </p>
      )}
    </section>
  )
}

export function AccountPage({ me }: { me: User }) {
  const tokens = usePoll(api.tokens, 60000, [])
  const users = usePoll(() => (me.is_admin ? api.users() : Promise.resolve([] as User[])), 60000, [])
  const [pw, setPw] = useState({ current: '', next: '' })
  const [tokenName, setTokenName] = useState('')
  const [newToken, setNewToken] = useState<string | null>(null)
  const [nu, setNu] = useState({ username: '', password: '', admin: false })
  const a = useAction()
  const now = useNow(30000)
  return (
    <>
      <h1 className="page-title">{me.username}</h1>
      <Err text={a.error} />
      <section className="settings">
        <h2>Access tokens</h2>
        <p className="muted small-text">Use a token as the password for git over HTTPS, or as a Bearer token for the API.</p>
        {newToken && (
          <div className="token-box">
            <p>Copy it now, because it won't be shown again.</p>
            <pre>{newToken}</pre>
            <button className="secondary small" onClick={() => setNewToken(null)}>
              Done
            </button>
          </div>
        )}
        {tokens.data?.map((t) => (
          <div key={t.id} className="inline">
            <strong>{t.name}</strong>
            <span className="muted small-text">
              created {ago(t.created_at, now)}
              {t.last_used ? ` · used ${ago(t.last_used, now)}` : ' · never used'}
              {t.scoped ? ' · scoped' : ''}
            </span>
            <button className="danger small" onClick={() => a.run(async () => { await api.deleteToken(t.id); tokens.reload() })}>
              Revoke
            </button>
          </div>
        ))}
        <form
          className="inline"
          onSubmit={(e) => {
            e.preventDefault()
            a.run(async () => {
              setNewToken((await api.createToken(tokenName)).token)
              setTokenName('')
              tokens.reload()
            })
          }}
        >
          <input placeholder="laptop" value={tokenName} onChange={(e) => setTokenName(e.target.value)} required />
          <button type="submit" className="secondary">
            New token
          </button>
        </form>
      </section>
      <section className="settings">
        <h2>Password</h2>
        <form
          className="inline"
          onSubmit={(e) => {
            e.preventDefault()
            a.run(async () => {
              await api.changePassword(pw.current, pw.next)
              setPw({ current: '', next: '' })
            })
          }}
        >
          <input type="password" placeholder="current" value={pw.current} onChange={(e) => setPw({ ...pw, current: e.target.value })} required />
          <input type="password" placeholder="new (8+ characters)" minLength={8} value={pw.next} onChange={(e) => setPw({ ...pw, next: e.target.value })} required />
          <button type="submit" className="secondary">
            Change
          </button>
        </form>
      </section>
      {me.is_admin && <CloudRunners />}
      {me.is_admin && (
        <section className="settings">
          <h2>Users</h2>
          <table className="runs">
            <tbody>
              {users.data?.map((u) => (
                <tr key={u.id} className="static">
                  <td>
                    {u.kind === 'bot' ? '🤖 ' : ''}
                    <strong>{u.username}</strong>
                    {u.is_admin && <span className="label">admin</span>}
                  </td>
                  <td className="muted right">{ago(u.created_at, now)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <form
            className="inline"
            onSubmit={(e) => {
              e.preventDefault()
              a.run(async () => {
                await api.createUser(nu.username, nu.password, nu.admin)
                setNu({ username: '', password: '', admin: false })
                users.reload()
              })
            }}
          >
            <input placeholder="username" value={nu.username} onChange={(e) => setNu({ ...nu, username: e.target.value })} required />
            <input type="password" placeholder="password" minLength={8} value={nu.password} onChange={(e) => setNu({ ...nu, password: e.target.value })} required />
            <label className="check">
              <input type="checkbox" checked={nu.admin} onChange={(e) => setNu({ ...nu, admin: e.target.checked })} /> admin
            </label>
            <button type="submit" className="secondary">
              Add user
            </button>
          </form>
        </section>
      )}
    </>
  )
}
