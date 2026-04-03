import { useReducer, useEffect, useRef } from 'react'
import './App.css'

type Phase = 'queued' | 'cloning' | 'building' | 'pushing' | 'done' | 'failed'

interface BranchState {
  branch: string
  phase: Phase
  image?: string
  elapsedMs?: number
  error?: string
}

interface Summary {
  succeeded: number
  failed: number
  elapsedMs: number
}

interface AppState {
  repo: string
  branches: string[]
  states: Record<string, BranchState>
  logs: Record<string, string[]>
  summary: Summary | null
}

type Action =
  | { type: 'init'; repo: string; branches: string[]; states: BranchState[]; logs: Record<string, string[]> }
  | { type: 'update'; state: BranchState }
  | { type: 'log'; branch: string; line: string }
  | { type: 'done'; summary: Summary }

function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case 'init': {
      const states: Record<string, BranchState> = {}
      for (const s of action.states) states[s.branch] = s
      return { repo: action.repo, branches: action.branches, states, logs: action.logs, summary: null }
    }
    case 'update':
      return { ...state, states: { ...state.states, [action.state.branch]: action.state } }
    case 'log': {
      const prev = state.logs[action.branch] ?? []
      return { ...state, logs: { ...state.logs, [action.branch]: [...prev, action.line] } }
    }
    case 'done':
      return { ...state, summary: action.summary }
    default:
      return state
  }
}

const initialState: AppState = { repo: '', branches: [], states: {}, logs: {}, summary: null }

const PHASE_COLOR: Record<Phase, string> = {
  queued:   '#475569',
  cloning:  '#3b82f6',
  building: '#f59e0b',
  pushing:  '#8b5cf6',
  done:     '#22c55e',
  failed:   '#ef4444',
}

const PHASE_LABEL: Record<Phase, string> = {
  queued:   'queued',
  cloning:  'cloning',
  building: 'building',
  pushing:  'pushing',
  done:     'done',
  failed:   'failed',
}

const ACTIVE_PHASES: Phase[] = ['cloning', 'building', 'pushing']

function formatMs(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  const m = Math.floor(ms / 60000)
  const s = Math.floor((ms % 60000) / 1000)
  return `${m}m ${s}s`
}

function LogPanel({ lines }: { lines: string[] }) {
  const panelRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const el = panelRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [lines.length])

  if (lines.length === 0) return null

  return (
    <div className="log-panel" ref={panelRef}>
      <pre className="log-pre">{lines.join('\n')}</pre>
    </div>
  )
}

function BranchCard({ state, logs }: { state: BranchState; logs: string[] }) {
  const color = PHASE_COLOR[state.phase]
  const isActive = ACTIVE_PHASES.includes(state.phase)

  return (
    <div className="card" style={{ borderLeftColor: color }}>
      <div className="card-header">
        <span className="branch-name" title={state.branch}>{state.branch}</span>
        <span className={`badge${isActive ? ' badge-active' : ''}`} style={{ backgroundColor: color }}>
          {isActive && <span className="spinner" />}
          {PHASE_LABEL[state.phase]}
        </span>
      </div>
      {state.elapsedMs != null && state.elapsedMs > 0 && (
        <div className="elapsed">{formatMs(state.elapsedMs)}</div>
      )}
      {state.phase === 'done' && state.image && (
        <div className="image-name" title={state.image}>{state.image}</div>
      )}
      <LogPanel lines={logs} />
      {state.phase === 'failed' && state.error && (
        <pre className="error-pre">{state.error}</pre>
      )}
    </div>
  )
}

export default function App() {
  const [state, dispatch] = useReducer(reducer, initialState)

  useEffect(() => {
    const es = new EventSource('/events')
    es.onmessage = (e) => {
      try {
        const msg = JSON.parse(e.data) as { event: string; data: unknown }
        if (msg.event === 'init') {
          const d = msg.data as { repo: string; branches: string[]; states: BranchState[]; logs: Record<string, string[]> }
          dispatch({ type: 'init', repo: d.repo, branches: d.branches, states: d.states, logs: d.logs ?? {} })
        } else if (msg.event === 'update') {
          dispatch({ type: 'update', state: msg.data as BranchState })
        } else if (msg.event === 'log') {
          const d = msg.data as { branch: string; line: string }
          dispatch({ type: 'log', branch: d.branch, line: d.line })
        } else if (msg.event === 'done') {
          dispatch({ type: 'done', summary: msg.data as Summary })
        }
      } catch {
        // ignore parse errors
      }
    }
    return () => es.close()
  }, [])

  const total = state.branches.length
  const completed = state.branches.filter(
    (b) => state.states[b]?.phase === 'done' || state.states[b]?.phase === 'failed',
  ).length
  const progress = total > 0 ? (completed / total) * 100 : 0

  return (
    <div className="app">
      <header className="header">
        <div className="header-top">
          <h1 className="title">moloko</h1>
          <div className="header-meta">
            {state.repo && <span className="repo-url">{state.repo}</span>}
            <span className="progress-label">
              {state.summary
                ? `${state.summary.succeeded} ok, ${state.summary.failed} failed — ${formatMs(state.summary.elapsedMs)}`
                : total > 0
                ? `${completed} / ${total}`
                : ''}
            </span>
          </div>
        </div>
        <div className="progress-bar-track">
          <div
            className={`progress-bar-fill${state.summary ? ' progress-done' : ''}`}
            style={{ width: `${progress}%` }}
          />
        </div>
      </header>

      <main className="grid">
        {state.branches.map((b) =>
          state.states[b] ? (
            <BranchCard key={b} state={state.states[b]} logs={state.logs[b] ?? []} />
          ) : null,
        )}
      </main>
    </div>
  )
}
