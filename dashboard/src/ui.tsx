import { type ReactNode, useState } from 'react'
import type { ReviewComment } from './api'

export function Badge({ status }: { status: string }) {
  return <span className={`badge ${status}`}>{status.replace('_', ' ')}</span>
}

export function Err({ text }: { text: string | null }) {
  return text ? <span className="error">{text}</span> : null
}

/** Plain text with issue keys (ABC-12) turned into links. */
export function Text({ body, pid }: { body: string; pid?: number }) {
  if (!pid) return <div className="text">{body}</div>
  const parts = body.split(/\b([A-Z][A-Z0-9]{1,9}-\d+)\b/)
  return (
    <div className="text">
      {parts.map((p, i) =>
        i % 2 ? (
          <a key={i} className="link" href={`#/p/${pid}/issues/${p.split('-')[1]}`}>
            {p}
          </a>
        ) : (
          p
        ),
      )}
    </div>
  )
}

export function Tabs({ tabs, current }: { tabs: [string, string][]; current: string }) {
  return (
    <nav className="tabs">
      {tabs.map(([href, label]) => (
        <a key={href} href={href} className={label === current ? 'active' : ''}>
          {label}
        </a>
      ))}
    </nav>
  )
}

interface Line {
  kind: 'ctx' | 'add' | 'del' | 'hunk'
  text: string
  old?: number
  new?: number
}

/** Split a unified diff into files with numbered lines. */
export function parseDiff(diff: string) {
  const files: { path: string; lines: Line[] }[] = []
  let cur: { path: string; lines: Line[] } | null = null
  let o = 0
  let n = 0
  for (const l of diff.split('\n')) {
    if (l.startsWith('diff --git ')) {
      cur = { path: l.split(' b/').pop() ?? '', lines: [] }
      files.push(cur)
    } else if (!cur || l.startsWith('index ') || l.startsWith('--- ') || l.startsWith('+++ ') || /^(new|deleted) file mode|^similarity|^rename |^old mode|^new mode/.test(l)) {
      continue
    } else if (l.startsWith('@@')) {
      const m = l.match(/@@ -(\d+)(?:,\d+)? \+(\d+)/)
      o = Number(m?.[1] ?? 0)
      n = Number(m?.[2] ?? 0)
      cur.lines.push({ kind: 'hunk', text: l })
    } else if (l.startsWith('+')) cur.lines.push({ kind: 'add', text: l.slice(1), new: n++ })
    else if (l.startsWith('-')) cur.lines.push({ kind: 'del', text: l.slice(1), old: o++ })
    else if (l.startsWith(' ')) cur.lines.push({ kind: 'ctx', text: l.slice(1), old: o++, new: n++ })
    else if (l.startsWith('Binary')) cur.lines.push({ kind: 'hunk', text: l })
  }
  return files
}

/**
 * A unified diff. With `onComment`, clicking a line number opens a comment box,
 * and `comments` are shown under the line they are anchored to.
 */
export function Diff({
  diff,
  comments = [],
  onComment,
  renderThread,
}: {
  diff: string
  comments?: ReviewComment[]
  onComment?: (path: string, line: number, side: 'new' | 'old', body: string) => Promise<unknown>
  renderThread?: (c: ReviewComment) => ReactNode
}) {
  const [open, setOpen] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const files = parseDiff(diff)
  if (!diff.trim()) return <p className="muted">No changes.</p>
  return (
    <div>
      {files.map((f) => (
        <section className="diff-file" key={f.path}>
          <div className="diff-head">{f.path}</div>
          <table className="diff">
            <tbody>
              {f.lines.map((l, i) => {
                const side = l.kind === 'del' ? 'old' : 'new'
                const line = side === 'old' ? l.old : l.new
                const key = `${f.path}:${side}:${line}`
                const here = comments.filter((c) => c.parent_id == null && c.path === f.path && c.line === line && (c.side ?? 'new') === side)
                return [
                  <tr key={i} className={l.kind}>
                    <td className="ln" onClick={() => onComment && line && setOpen(open === key ? null : key)}>
                      {l.old ?? ''}
                    </td>
                    <td className="ln" onClick={() => onComment && line && setOpen(open === key ? null : key)}>
                      {l.new ?? ''}
                    </td>
                    <td className="code">
                      {l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : l.kind === 'ctx' ? ' ' : ''}
                      {l.text}
                    </td>
                  </tr>,
                  here.length > 0 && renderThread && (
                    <tr key={`${i}c`} className="thread-row">
                      <td colSpan={3}>{here.map((c) => renderThread(c))}</td>
                    </tr>
                  ),
                  open === key && onComment && line && (
                    <tr key={`${i}n`} className="thread-row">
                      <td colSpan={3}>
                        <form
                          className="comment-form"
                          onSubmit={async (e) => {
                            e.preventDefault()
                            await onComment(f.path, line, side, draft)
                            setDraft('')
                            setOpen(null)
                          }}
                        >
                          <textarea autoFocus value={draft} onChange={(e) => setDraft(e.target.value)} placeholder="Comment on this line" />
                          <div className="inline">
                            <button type="submit" disabled={!draft.trim()}>
                              Comment
                            </button>
                            <button type="button" className="secondary" onClick={() => setOpen(null)}>
                              Cancel
                            </button>
                          </div>
                        </form>
                      </td>
                    </tr>
                  ),
                ]
              })}
            </tbody>
          </table>
        </section>
      ))}
    </div>
  )
}
