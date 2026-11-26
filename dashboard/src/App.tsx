import { useEffect, useState } from 'react'
import { api, setOnUnauthorized, type User } from './api'
import { AgentsPage, ImagesPage, RunPage, RunsPage } from './ci'
import { CodeTab } from './code'
import { BoardTab, IssuePage, IssuesTab, Login, ProjectShell, ProjectsPage } from './forge'
import { usePoll, useRoute } from './hooks'
import { NewReview, ReviewPage, ReviewsTab } from './reviews'
import { AccountPage, AiTab, SettingsTab } from './settings'

export default function App() {
  const [auth, setAuth] = useState<{ user: User | null; setup_required: boolean } | null>(null)
  useEffect(() => {
    api.me().then(setAuth, () => setAuth({ user: null, setup_required: false }))
    setOnUnauthorized(() => setAuth({ user: null, setup_required: false }))
  }, [])
  if (!auth) return null
  if (!auth.user) return <Login setup={auth.setup_required} onDone={(user) => setAuth({ user, setup_required: false })} />
  return <Shell me={auth.user} onLogout={() => api.logout().finally(() => setAuth({ user: null, setup_required: false }))} />
}

function Shell({ me, onLogout }: { me: User; onLogout: () => void }) {
  const { parts, query } = useRoute()
  const top = parts[0] ?? ''
  const nav: [string, string, boolean][] = [
    ['#/', 'Projects', top === '' || top === 'p'],
    ['#/runs', 'Runs', top === 'runs'],
    ['#/agents', 'Deploy agents', top === 'agents'],
    ['#/images', 'Images', top === 'images'],
  ]
  return (
    <div className="shell">
      <header className="topbar">
        <a href="#/" className="logo">
          moloko
        </a>
        <nav>
          {nav.map(([href, label, active]) => (
            <a key={href} href={href} className={active ? 'active' : ''}>
              {label}
            </a>
          ))}
        </nav>
        <span className="grow" />
        <a href="#/account" className={top === 'account' ? 'active' : 'muted'}>
          {me.username}
        </a>
        <button className="secondary small" onClick={onLogout}>
          Sign out
        </button>
      </header>
      <main>
        {top === 'runs' && parts[1] ? (
          <RunPage id={Number(parts[1])} />
        ) : top === 'runs' ? (
          <RunsPage />
        ) : top === 'agents' ? (
          <AgentsPage />
        ) : top === 'images' ? (
          <ImagesPage />
        ) : top === 'account' ? (
          <AccountPage me={me} />
        ) : top === 'p' && parts[1] ? (
          <Project pid={Number(parts[1])} rest={parts.slice(2)} query={query} />
        ) : (
          <ProjectsPage />
        )}
      </main>
    </div>
  )
}

function Project({ pid, rest, query }: { pid: number; rest: string[]; query: URLSearchParams }) {
  const d = usePoll(() => api.project(pid), 30000, [pid])
  if (d.error && !d.data) return <p className="error">{d.error}</p>
  if (!d.data || d.data.project.id !== pid) return <p className="muted">Loading…</p>
  const detail = d.data
  const [section = '', id] = rest
  const tab =
    { '': 'Issues', issues: 'Issues', board: 'Board', reviews: 'Reviews', code: 'Code', runs: 'Runs', ai: 'AI agents', settings: 'Settings' }[section] ?? 'Issues'
  const body =
    section === 'issues' && id ? (
      <IssuePage detail={detail} number={Number(id)} />
    ) : section === 'board' ? (
      <BoardTab detail={detail} />
    ) : section === 'reviews' && id === 'new' ? (
      <NewReview detail={detail} query={query} />
    ) : section === 'reviews' && id ? (
      <ReviewPage detail={detail} number={Number(id)} query={query} />
    ) : section === 'reviews' ? (
      <ReviewsTab detail={detail} />
    ) : section === 'code' ? (
      <CodeTab detail={detail} repoId={id ? Number(id) : undefined} query={query} />
    ) : section === 'runs' ? (
      <RunsPage fixedProject={pid} />
    ) : section === 'ai' ? (
      <AiTab detail={detail} />
    ) : section === 'settings' ? (
      <SettingsTab detail={detail} reload={d.reload} />
    ) : (
      <IssuesTab key={query.get('q') ?? ''} detail={detail} query={query} />
    )
  return (
    <ProjectShell detail={detail} tab={tab}>
      {body}
    </ProjectShell>
  )
}
