//! Task tracking: issues with keys like `MOL-42`, comments, links, a board, cycles, and
//! automation that moves issues when branches, commits, runs, reviews and deploys happen.

use crate::api::{ApiError, ApiResult, AppState, bad_request, need, not_found};
use crate::auth::{Actor, Role};
use crate::db::{Db, Status, now_ms};
use crate::engine::{Engine, RunCtx};
use crate::repos::{RefUpdate, Repo};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cycles (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    starts_at INTEGER NOT NULL,
    ends_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS issues (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    type TEXT NOT NULL DEFAULT 'task',
    status TEXT NOT NULL DEFAULT 'todo',
    priority TEXT NOT NULL DEFAULT 'none',
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    assignee_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    reporter_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    parent_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
    cycle_id INTEGER REFERENCES cycles(id) ON DELETE SET NULL,
    estimate INTEGER,
    due TEXT,
    labels TEXT NOT NULL DEFAULT '[]',
    rank REAL NOT NULL DEFAULT 0,
    closed_at INTEGER,
    released_at INTEGER,
    released_env TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (project_id, number)
);
CREATE TABLE IF NOT EXISTS issue_events (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    data TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS issue_events_issue ON issue_events(issue_id, id);
CREATE TABLE IF NOT EXISTS issue_links (
    from_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    to_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    PRIMARY KEY (from_id, to_id, kind)
);
CREATE TABLE IF NOT EXISTS issue_refs (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    repo_id INTEGER,
    ref TEXT NOT NULL,
    sha TEXT,
    data TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (issue_id, kind, ref)
);
"#;

pub const STATUSES: &[&str] = &["backlog", "todo", "in_progress", "in_review", "done", "cancelled"];
pub const TYPES: &[&str] = &["task", "bug", "story", "epic"];
pub const PRIORITIES: &[&str] = &["none", "low", "medium", "high", "urgent"];
const LINK_KINDS: &[&str] = &["blocks", "relates", "duplicates"];

pub fn valid_key(k: &str) -> bool {
    (1..=10).contains(&k.len())
        && k.starts_with(|c: char| c.is_ascii_uppercase())
        && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Issue numbers mentioned as `KEY-123` in `text` (case-insensitive, so branch names like
/// `mol-12-fix` count too).
pub fn find_keys(text: &str, key: &str) -> Vec<i64> {
    let (t, k) = (text.as_bytes(), key.as_bytes());
    let mut out = Vec::new();
    let mut i = 0;
    while i + k.len() + 1 < t.len() + 1 {
        let boundary = i == 0 || !t[i - 1].is_ascii_alphanumeric();
        if boundary
            && t.len() > i + k.len()
            && t[i..i + k.len()].eq_ignore_ascii_case(k)
            && t[i + k.len()] == b'-'
        {
            let start = i + k.len() + 1;
            let end = t[start..].iter().position(|b| !b.is_ascii_digit()).map_or(t.len(), |p| start + p);
            if end > start
                && let Ok(n) = std::str::from_utf8(&t[start..end]).unwrap().parse::<i64>()
                && !out.contains(&n)
            {
                out.push(n);
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Numbers closed by a commit message: `fixes MOL-1`, `closes MOL-2`, `resolves MOL-3`.
pub fn closing_keys(text: &str, key: &str) -> Vec<i64> {
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    for word in ["fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(word) {
            let at = from + pos;
            from = at + word.len();
            let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
            let rest = &text[from..];
            let trimmed = rest.trim_start_matches([' ', ':']);
            if before_ok && rest.len() != trimmed.len() {
                if let Some(&n) = find_keys(&trimmed[..trimmed.len().min(key.len() + 12)], key).first()
                    && !out.contains(&n)
                {
                    out.push(n);
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub id: i64,
    pub project_id: i64,
    pub number: i64,
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub priority: String,
    pub title: String,
    pub body: String,
    pub assignee: Option<String>,
    pub assignee_id: Option<i64>,
    pub assignee_kind: Option<String>,
    pub reporter: Option<String>,
    pub parent_id: Option<i64>,
    pub cycle_id: Option<i64>,
    pub estimate: Option<i64>,
    pub due: Option<String>,
    pub labels: Vec<String>,
    pub rank: f64,
    pub closed_at: Option<i64>,
    pub released_at: Option<i64>,
    pub released_env: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const ISSUE_COLS: &str = "i.id, i.project_id, i.number, p.key || '-' || i.number, i.type, i.status, i.priority, i.title, i.body,
    a.username, i.assignee_id, a.kind, r.username, i.parent_id, i.cycle_id, i.estimate, i.due, i.labels, i.rank,
    i.closed_at, i.released_at, i.released_env, i.created_at, i.updated_at";
const ISSUE_FROM: &str = "issues i JOIN projects p ON p.id = i.project_id
    LEFT JOIN users a ON a.id = i.assignee_id LEFT JOIN users r ON r.id = i.reporter_id";

fn map_issue(r: &rusqlite::Row) -> rusqlite::Result<Issue> {
    let labels: String = r.get(17)?;
    Ok(Issue {
        id: r.get(0)?,
        project_id: r.get(1)?,
        number: r.get(2)?,
        key: r.get(3)?,
        kind: r.get(4)?,
        status: r.get(5)?,
        priority: r.get(6)?,
        title: r.get(7)?,
        body: r.get(8)?,
        assignee: r.get(9)?,
        assignee_id: r.get(10)?,
        assignee_kind: r.get(11)?,
        reporter: r.get(12)?,
        parent_id: r.get(13)?,
        cycle_id: r.get(14)?,
        estimate: r.get(15)?,
        due: r.get(16)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        rank: r.get(18)?,
        closed_at: r.get(19)?,
        released_at: r.get(20)?,
        released_env: r.get(21)?,
        created_at: r.get(22)?,
        updated_at: r.get(23)?,
    })
}

/// A parsed search like `status:open assignee:me label:backend login bug`.
#[derive(Debug, Default, PartialEq)]
pub struct Filter {
    pub status: Vec<String>,
    pub open: Option<bool>,
    pub kind: Option<String>,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub label: Vec<String>,
    pub cycle: Option<i64>,
    pub parent: Option<i64>,
    pub text: Vec<String>,
}

impl Filter {
    pub fn parse(q: &str, me: &str) -> Filter {
        let mut f = Filter::default();
        for tok in q.split_whitespace() {
            match tok.split_once(':') {
                Some(("status", "open")) | Some(("is", "open")) => f.open = Some(true),
                Some(("status", "closed")) | Some(("is", "closed")) => f.open = Some(false),
                Some(("status", v)) => f.status.extend(v.split(',').map(str::to_string)),
                Some(("type", v)) => f.kind = Some(v.into()),
                Some(("priority", v)) => f.priority = Some(v.into()),
                Some(("assignee", "me")) => f.assignee = Some(me.into()),
                Some(("assignee", v)) => f.assignee = Some(v.trim_start_matches('@').into()),
                Some(("label", v)) => f.label.push(v.into()),
                Some(("cycle", v)) => f.cycle = v.parse().ok(),
                Some(("parent", v)) => f.parent = v.parse().ok(),
                _ => f.text.push(tok.to_string()),
            }
        }
        f
    }
}

#[derive(Default, Deserialize)]
pub struct IssueFields {
    pub title: Option<String>,
    pub body: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    /// Username, or empty string to unassign.
    pub assignee: Option<String>,
    /// Issue id, or 0 to clear.
    pub parent_id: Option<i64>,
    /// Cycle id, or 0 to clear.
    pub cycle_id: Option<i64>,
    pub estimate: Option<i64>,
    pub due: Option<String>,
    pub labels: Option<Vec<String>>,
    pub rank: Option<f64>,
}

impl Db {
    pub fn issue(&self, id: i64) -> Result<Option<Issue>> {
        let c = self.conn();
        Ok(c.query_row(&format!("SELECT {ISSUE_COLS} FROM {ISSUE_FROM} WHERE i.id = ?1"), [id], map_issue)
            .optional()?)
    }

    pub fn issue_by_number(&self, project_id: i64, number: i64) -> Result<Option<Issue>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {ISSUE_COLS} FROM {ISSUE_FROM} WHERE i.project_id = ?1 AND i.number = ?2"),
            params![project_id, number],
            map_issue,
        )
        .optional()?)
    }

    pub fn issues(&self, project_id: i64, f: &Filter, limit: i64) -> Result<Vec<Issue>> {
        use rusqlite::types::Value as V;
        let mut sql = format!("SELECT {ISSUE_COLS} FROM {ISSUE_FROM} WHERE i.project_id = ?");
        let mut args: Vec<V> = vec![project_id.into()];
        if !f.status.is_empty() {
            sql.push_str(&format!(" AND i.status IN ({})", vec!["?"; f.status.len()].join(",")));
            args.extend(f.status.iter().map(|s| V::from(s.clone())));
        }
        match f.open {
            Some(true) => sql.push_str(" AND i.status NOT IN ('done', 'cancelled')"),
            Some(false) => sql.push_str(" AND i.status IN ('done', 'cancelled')"),
            None => {}
        }
        if let Some(k) = &f.kind {
            sql.push_str(" AND i.type = ?");
            args.push(k.clone().into());
        }
        if let Some(p) = &f.priority {
            sql.push_str(" AND i.priority = ?");
            args.push(p.clone().into());
        }
        match f.assignee.as_deref() {
            Some("none") => sql.push_str(" AND i.assignee_id IS NULL"),
            Some(a) => {
                sql.push_str(" AND a.username = ? COLLATE NOCASE");
                args.push(a.to_string().into());
            }
            None => {}
        }
        for l in &f.label {
            sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(i.labels) WHERE value = ?)");
            args.push(l.clone().into());
        }
        if let Some(c) = f.cycle {
            sql.push_str(" AND i.cycle_id = ?");
            args.push(c.into());
        }
        if let Some(p) = f.parent {
            sql.push_str(" AND i.parent_id = ?");
            args.push(p.into());
        }
        for t in &f.text {
            let like = format!("%{}%", t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            sql.push_str(" AND (i.title LIKE ? ESCAPE '\\' OR i.body LIKE ? ESCAPE '\\')");
            args.push(like.clone().into());
            args.push(like.into());
        }
        sql.push_str(" ORDER BY i.rank, i.id DESC LIMIT ?");
        args.push(limit.into());
        let c = self.conn();
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), map_issue)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn create_issue(&self, project_id: i64, reporter: Option<i64>, title: &str, f: &IssueFields) -> Result<i64> {
        let c = self.conn();
        let number: i64 = c.query_row(
            "SELECT COALESCE(MAX(number), 0) + 1 FROM issues WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )?;
        let now = now_ms();
        c.execute(
            "INSERT INTO issues (project_id, number, title, body, type, status, priority, reporter_id, labels, estimate, due, rank, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                project_id,
                number,
                title,
                f.body.clone().unwrap_or_default(),
                f.kind.clone().unwrap_or_else(|| "task".into()),
                f.status.clone().unwrap_or_else(|| "todo".into()),
                f.priority.clone().unwrap_or_else(|| "none".into()),
                reporter,
                serde_json::to_string(&f.labels.clone().unwrap_or_default())?,
                f.estimate,
                f.due,
                f.rank.unwrap_or(number as f64),
                now
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Set one column and log the change. Returns false when nothing changed.
    pub fn set_issue_field(&self, issue_id: i64, actor: Option<i64>, field: &str, value: Value) -> Result<bool> {
        const ALLOWED: &[&str] = &[
            "title", "body", "type", "status", "priority", "assignee_id", "parent_id", "cycle_id", "estimate", "due", "labels", "rank",
        ];
        anyhow::ensure!(ALLOWED.contains(&field), "unknown field {field}");
        let c = self.conn();
        let old: Value = c.query_row(&format!("SELECT json_quote({field}) FROM issues WHERE id = ?1"), [issue_id], |r| {
            let s: String = r.get(0)?;
            Ok(serde_json::from_str(&s).unwrap_or(Value::Null))
        })?;
        let stored = if field == "labels" { Value::String(value.to_string()) } else { value.clone() };
        let old_cmp = if field == "labels" { serde_json::from_str(old.as_str().unwrap_or("[]")).unwrap_or(Value::Null) } else { old.clone() };
        if old_cmp == value {
            return Ok(false);
        }
        let sql_val: rusqlite::types::Value = match &stored {
            Value::Null => rusqlite::types::Value::Null,
            Value::Number(n) if n.is_i64() => n.as_i64().unwrap().into(),
            Value::Number(n) => n.as_f64().unwrap().into(),
            Value::String(s) => s.clone().into(),
            other => other.to_string().into(),
        };
        let now = now_ms();
        c.execute(&format!("UPDATE issues SET {field} = ?2, updated_at = ?3 WHERE id = ?1"), params![issue_id, sql_val, now])?;
        if field == "status" {
            let closed = matches!(value.as_str(), Some("done" | "cancelled"));
            c.execute(
                "UPDATE issues SET closed_at = CASE WHEN ?2 THEN COALESCE(closed_at, ?3) ELSE NULL END WHERE id = ?1",
                params![issue_id, closed, now],
            )?;
        }
        if field != "rank" {
            c.execute(
                "INSERT INTO issue_events (issue_id, actor_id, kind, data, created_at) VALUES (?1, ?2, 'field', ?3, ?4)",
                params![issue_id, actor, json!({ "field": field, "from": old_cmp, "to": value }).to_string(), now],
            )?;
        }
        Ok(true)
    }

    pub fn add_issue_event(&self, issue_id: i64, actor: Option<i64>, kind: &str, body: &str, data: Option<Value>) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO issue_events (issue_id, actor_id, kind, body, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![issue_id, actor, kind, body, data.map(|d| d.to_string()), now_ms()],
        )?;
        c.execute("UPDATE issues SET updated_at = ?2 WHERE id = ?1", params![issue_id, now_ms()])?;
        Ok(c.last_insert_rowid())
    }

    pub fn issue_events(&self, issue_id: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT e.id, u.username, u.kind, e.kind, e.body, e.data, e.created_at FROM issue_events e
             LEFT JOIN users u ON u.id = e.actor_id WHERE e.issue_id = ?1 ORDER BY e.id",
        )?;
        let rows = st.query_map([issue_id], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "actor": r.get::<_, Option<String>>(1)?,
                "actor_kind": r.get::<_, Option<String>>(2)?,
                "kind": r.get::<_, String>(3)?,
                "body": r.get::<_, String>(4)?,
                "data": r.get::<_, Option<String>>(5)?.and_then(|d| serde_json::from_str::<Value>(&d).ok()),
                "created_at": r.get::<_, i64>(6)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record a branch, commit, run, review, merge or deploy on an issue. Re-recording updates it.
    pub fn set_issue_ref(&self, issue_id: i64, kind: &str, repo_id: Option<i64>, name: &str, sha: Option<&str>, data: Option<Value>) -> Result<bool> {
        let c = self.conn();
        let existed: bool = c.query_row(
            "SELECT EXISTS (SELECT 1 FROM issue_refs WHERE issue_id = ?1 AND kind = ?2 AND ref = ?3)",
            params![issue_id, kind, name],
            |r| r.get(0),
        )?;
        c.execute(
            "INSERT INTO issue_refs (issue_id, kind, repo_id, ref, sha, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (issue_id, kind, ref) DO UPDATE SET sha = excluded.sha, data = excluded.data",
            params![issue_id, kind, repo_id, name, sha, data.map(|d| d.to_string()), now_ms()],
        )?;
        Ok(!existed)
    }

    pub fn issue_refs(&self, issue_id: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT x.kind, x.repo_id, r.name, x.ref, x.sha, x.data, x.created_at FROM issue_refs x
             LEFT JOIN repos r ON r.id = x.repo_id WHERE x.issue_id = ?1 ORDER BY x.created_at",
        )?;
        let rows = st.query_map([issue_id], |r| {
            Ok(json!({
                "kind": r.get::<_, String>(0)?,
                "repo_id": r.get::<_, Option<i64>>(1)?,
                "repo": r.get::<_, Option<String>>(2)?,
                "ref": r.get::<_, String>(3)?,
                "sha": r.get::<_, Option<String>>(4)?,
                "data": r.get::<_, Option<String>>(5)?.and_then(|d| serde_json::from_str::<Value>(&d).ok()),
                "created_at": r.get::<_, i64>(6)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn link_issues(&self, from: i64, to: i64, kind: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO issue_links (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
            params![from, to, kind],
        )?;
        Ok(())
    }

    pub fn unlink_issues(&self, from: i64, to: i64, kind: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM issue_links WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
            params![from, to, kind],
        )? > 0)
    }

    pub fn issue_links(&self, id: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT l.kind, 'out', p.key || '-' || i.number, i.id, i.title, i.status FROM issue_links l
               JOIN issues i ON i.id = l.to_id JOIN projects p ON p.id = i.project_id WHERE l.from_id = ?1
             UNION ALL
             SELECT l.kind, 'in', p.key || '-' || i.number, i.id, i.title, i.status FROM issue_links l
               JOIN issues i ON i.id = l.from_id JOIN projects p ON p.id = i.project_id WHERE l.to_id = ?1",
        )?;
        let rows = st.query_map([id], |r| {
            Ok(json!({
                "kind": r.get::<_, String>(0)?, "direction": r.get::<_, String>(1)?, "key": r.get::<_, String>(2)?,
                "id": r.get::<_, i64>(3)?, "title": r.get::<_, String>(4)?, "status": r.get::<_, String>(5)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn create_cycle(&self, project_id: i64, name: &str, starts: i64, ends: i64) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO cycles (project_id, name, starts_at, ends_at) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, name, starts, ends],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn cycles(&self, project_id: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, name, starts_at, ends_at FROM cycles WHERE project_id = ?1 ORDER BY starts_at DESC")?;
        let rows = st.query_map([project_id], |r| {
            Ok(json!({ "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "starts_at": r.get::<_, i64>(2)?, "ends_at": r.get::<_, i64>(3)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Points (or issue count when unestimated) still open at the end of each day of a cycle.
    pub fn burndown(&self, cycle_id: i64) -> Result<Value> {
        let c = self.conn();
        let (starts, ends): (i64, i64) =
            c.query_row("SELECT starts_at, ends_at FROM cycles WHERE id = ?1", [cycle_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut st = c.prepare("SELECT COALESCE(estimate, 1), closed_at FROM issues WHERE cycle_id = ?1")?;
        let items: Vec<(i64, Option<i64>)> =
            st.query_map([cycle_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let total: i64 = items.iter().map(|i| i.0).sum();
        const DAY: i64 = 86_400_000;
        let mut points = Vec::new();
        let mut day = starts;
        let last = ends.min(now_ms());
        while day <= last + DAY - 1 {
            let end_of_day = day + DAY;
            let open: i64 = items.iter().filter(|(_, closed)| closed.is_none_or(|t| t >= end_of_day)).map(|i| i.0).sum();
            points.push(json!({ "day": day, "remaining": open }));
            day += DAY;
        }
        Ok(json!({ "total": total, "starts_at": starts, "ends_at": ends, "points": points }))
    }
}

// ---------- automation ----------

fn branch_slug(title: &str) -> String {
    let s = crate::repos::slug(title);
    let mut out = String::new();
    for part in s.split('-') {
        if out.len() + part.len() > 40 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(part);
    }
    out
}

/// The branch name moloko suggests for an issue: `MOL-42-fix-login-redirect`.
pub fn branch_name(issue: &Issue) -> String {
    let slug = branch_slug(&issue.title);
    if slug.is_empty() { issue.key.clone() } else { format!("{}-{slug}", issue.key) }
}

/// Move an issue forward only: automation never reopens or moves an issue backwards.
pub fn advance(db: &Db, issue: &Issue, to: &str, why: &str) -> Result<()> {
    let rank = |s: &str| STATUSES.iter().position(|x| *x == s).unwrap_or(0);
    if issue.status == "cancelled" || rank(&issue.status) >= rank(to) {
        return Ok(());
    }
    if db.set_issue_field(issue.id, None, "status", json!(to))? {
        db.add_issue_event(issue.id, None, "auto", why, Some(json!({ "status": to })))?;
    }
    Ok(())
}

pub async fn on_ref_update(engine: &Arc<Engine>, repo: &Repo, actor_id: Option<i64>, u: &RefUpdate, path: &std::path::Path) -> Result<()> {
    let Some(project) = engine.db.project(repo.project_id)? else { return Ok(()) };
    let Some(branch) = u.branch() else { return Ok(()) };
    let is_default = branch == repo.default_branch;
    let _ = actor_id;
    if u.is_delete() {
        return Ok(());
    }
    if !is_default {
        for n in find_keys(branch, &project.key) {
            let Some(issue) = engine.db.issue_by_number(project.id, n)? else { continue };
            if engine.db.set_issue_ref(issue.id, "branch", Some(repo.id), &format!("{}:{branch}", repo.name), Some(&u.new), None)? {
                engine.db.add_issue_event(issue.id, None, "auto", &format!("Branch {branch} pushed to {}", repo.name), None)?;
            }
            advance(&engine.db, &issue, "in_progress", &format!("Work started on {branch}"))?;
        }
    }
    // Commits new to this ref.
    let range = if crate::git::is_zero(&u.old) {
        if is_default { u.new.clone() } else { format!("{} ^refs/heads/{}", u.new, repo.default_branch) }
    } else {
        format!("{}..{}", u.old, u.new)
    };
    let mut args = vec!["log", "-n200", "--format=%H%x00%s%x00%b%x1e"];
    let parts: Vec<&str> = range.split(' ').collect();
    args.extend(parts.iter().copied());
    args.push("--");
    let Ok(out) = crate::git::git(&args, Some(path)).await else { return Ok(()) };
    for rec in out.split('\x1e').map(|r| r.trim_start_matches('\n')).filter(|r| !r.is_empty()) {
        let f: Vec<&str> = rec.splitn(3, '\0').collect();
        if f.len() < 2 {
            continue;
        }
        let msg = format!("{}\n{}", f[1], f.get(2).unwrap_or(&""));
        for n in find_keys(&msg, &project.key) {
            if let Some(issue) = engine.db.issue_by_number(project.id, n)? {
                engine.db.set_issue_ref(issue.id, "commit", Some(repo.id), f[0], Some(f[0]), Some(json!({ "subject": f[1], "repo": repo.name })))?;
            }
        }
        if is_default {
            for n in closing_keys(&msg, &project.key) {
                if let Some(issue) = engine.db.issue_by_number(project.id, n)? {
                    mark_merged(&engine.db, &issue, repo, f[0], &format!("Fixed by {} on {}", &f[0][..7], repo.default_branch))?;
                }
            }
        }
    }
    Ok(())
}

/// The change for an issue landed on the default branch: done, waiting for a deploy.
pub fn mark_merged(db: &Db, issue: &Issue, repo: &Repo, sha: &str, why: &str) -> Result<()> {
    db.set_issue_ref(issue.id, "merge", Some(repo.id), &format!("{}:{sha}", repo.name), Some(sha), None)?;
    advance(db, issue, "done", why)
}

pub async fn on_run_finished(engine: &Arc<Engine>, ctx: &Arc<RunCtx>, status: Status) -> Result<()> {
    let Some(project) = engine.db.project(ctx.project_id)? else { return Ok(()) };
    let branch = ctx.git_ref.strip_prefix("refs/heads/").unwrap_or(&ctx.git_ref);
    for n in find_keys(branch, &project.key) {
        if let Some(issue) = engine.db.issue_by_number(project.id, n)? {
            engine.db.set_issue_ref(
                issue.id,
                "run",
                Some(ctx.repo.id),
                &format!("{}:{branch}", ctx.repo.name),
                Some(&ctx.sha),
                Some(json!({ "run_id": ctx.run_id, "status": status.as_str() })),
            )?;
        }
    }
    // Deploys: agent jobs that passed. A deploy to `prod` marks merged issues as released.
    if status != Status::Success {
        return Ok(());
    }
    let jobs = engine.db.jobs(ctx.run_id)?;
    let mut envs: Vec<String> = Vec::new();
    for j in jobs.iter().filter(|j| j.status == "success") {
        let base = j.name.split('[').next().unwrap_or(&j.name);
        if let Some(spec) = ctx.pipeline.jobs.get(base)
            && let Some(label) = spec.agent.first()
            && !envs.contains(label)
        {
            envs.push(label.clone());
        }
    }
    if envs.is_empty() {
        return Ok(());
    }
    let path = engine.git.path(ctx.repo.id);
    let pending: Vec<(i64, String)> = {
        let c = engine.db.conn();
        let mut st = c.prepare(
            "SELECT i.id, x.sha FROM issues i JOIN issue_refs x ON x.issue_id = i.id
             WHERE i.project_id = ?1 AND x.kind = 'merge' AND x.repo_id = ?2",
        )?;
        st.query_map(params![ctx.project_id, ctx.repo.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (issue_id, sha) in pending {
        if !engine.git.is_ancestor(&path, &sha, &ctx.sha).await {
            continue;
        }
        for env in &envs {
            if engine.db.set_issue_ref(issue_id, "deploy", Some(ctx.repo.id), env, Some(&ctx.sha), Some(json!({ "run_id": ctx.run_id })))? {
                engine.db.add_issue_event(issue_id, None, "auto", &format!("Deployed to {env} by run #{}", ctx.run_id), None)?;
            }
            if env == "prod" || env == "production" {
                engine.db.conn().execute(
                    "UPDATE issues SET released_at = COALESCE(released_at, ?2), released_env = COALESCE(released_env, ?3) WHERE id = ?1",
                    params![issue_id, now_ms(), env],
                )?;
            }
        }
    }
    Ok(())
}

// ---------- API ----------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects/{id}/issues", get(list).post(create))
        .route("/api/projects/{id}/issues/{number}", get(by_number))
        .route("/api/projects/{id}/cycles", get(list_cycles).post(create_cycle))
        .route("/api/projects/{id}/import", post(import))
        .route("/api/cycles/{id}/burndown", get(burndown))
        .route("/api/issues/{id}", get(detail).patch(update))
        .route("/api/issues/{id}/comments", post(comment))
        .route("/api/issues/{id}/links", post(add_link))
        .route("/api/issues/{id}/links/{to}/{kind}", delete(remove_link))
        .route("/api/issues/{id}/branch", post(make_branch))
}

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    let f = Filter::parse(q.q.as_deref().unwrap_or(""), &actor.user.username);
    Ok(Json(s.engine.db.issues(id, &f, q.limit.unwrap_or(500).clamp(1, 2000))?))
}

fn validate(f: &IssueFields) -> ApiResult<()> {
    let check = |v: &Option<String>, allowed: &[&str], name: &str| match v {
        Some(x) if !allowed.contains(&x.as_str()) => Err(bad_request(format!("{name} must be one of {}", allowed.join(", ")))),
        _ => Ok(()),
    };
    check(&f.kind, TYPES, "type")?;
    check(&f.status, STATUSES, "status")?;
    check(&f.priority, PRIORITIES, "priority")?;
    if f.title.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(bad_request("title can't be empty"));
    }
    Ok(())
}

fn resolve_assignee(s: &AppState, project_id: i64, name: &str) -> ApiResult<Option<i64>> {
    if name.is_empty() {
        return Ok(None);
    }
    let user = s.engine.db.user_by_name(name.trim_start_matches('@'))?.ok_or_else(|| bad_request(format!("no user {name}")))?;
    if s.engine.db.role(&user, project_id)?.is_none() {
        return Err(bad_request(format!("{name} is not a member of this project")));
    }
    Ok(Some(user.id))
}

/// Apply field changes; returns the changed field names.
fn apply_fields(s: &AppState, actor: &Actor, issue: &Issue, f: &IssueFields) -> ApiResult<Vec<&'static str>> {
    validate(f)?;
    let db = &s.engine.db;
    let me = Some(actor.user.id);
    let mut changed = Vec::new();
    let mut set = |field: &'static str, v: Value| -> ApiResult<()> {
        if db.set_issue_field(issue.id, me, field, v)? {
            changed.push(field);
        }
        Ok(())
    };
    if let Some(v) = &f.title { set("title", json!(v.trim()))?; }
    if let Some(v) = &f.body { set("body", json!(v))?; }
    if let Some(v) = &f.kind { set("type", json!(v))?; }
    if let Some(v) = &f.status { set("status", json!(v))?; }
    if let Some(v) = &f.priority { set("priority", json!(v))?; }
    if let Some(v) = &f.assignee { set("assignee_id", json!(resolve_assignee(s, issue.project_id, v)?))?; }
    if let Some(v) = f.parent_id {
        if v != 0 {
            let p = db.issue(v)?.filter(|p| p.project_id == issue.project_id && p.id != issue.id).ok_or_else(|| bad_request("bad parent"))?;
            if p.parent_id == Some(issue.id) {
                return Err(bad_request("an issue can't be its own grandparent"));
            }
        }
        set("parent_id", if v == 0 { Value::Null } else { json!(v) })?;
    }
    if let Some(v) = f.cycle_id { set("cycle_id", if v == 0 { Value::Null } else { json!(v) })?; }
    if let Some(v) = f.estimate { set("estimate", if v < 0 { Value::Null } else { json!(v) })?; }
    if let Some(v) = &f.due { set("due", if v.is_empty() { Value::Null } else { json!(v) })?; }
    if let Some(v) = &f.labels {
        let mut l: Vec<String> = v.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        l.sort();
        l.dedup();
        set("labels", json!(l))?;
    }
    if let Some(v) = f.rank { set("rank", json!(v))?; }
    Ok(changed)
}

async fn create(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(f): Json<IssueFields>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Write)?;
    validate(&f)?;
    let title = f.title.clone().unwrap_or_default();
    if title.trim().is_empty() {
        return Err(bad_request("title is required"));
    }
    let iid = s.engine.db.create_issue(id, Some(actor.user.id), title.trim(), &f)?;
    let issue = s.engine.db.issue(iid)?.ok_or_else(not_found)?;
    let rest = IssueFields { assignee: f.assignee.clone(), parent_id: f.parent_id, cycle_id: f.cycle_id, ..Default::default() };
    let changed = apply_fields(&s, &actor, &issue, &rest)?;
    let issue = s.engine.db.issue(iid)?.ok_or_else(not_found)?;
    if changed.contains(&"assignee_id") {
        crate::ai::on_assigned(&s.engine, &issue).await?;
    }
    Ok((StatusCode::CREATED, Json(issue)))
}

async fn issue_for(s: &AppState, actor: &Actor, id: i64, min: Role) -> ApiResult<Issue> {
    let issue = s.engine.db.issue(id)?.ok_or_else(not_found)?;
    need(s, actor, issue.project_id, min)?;
    Ok(issue)
}

async fn detail_json(s: &AppState, issue: Issue) -> ApiResult<Value> {
    let db = &s.engine.db;
    let children = db.issues(issue.project_id, &Filter { parent: Some(issue.id), ..Default::default() }, 500)?;
    let parent = match issue.parent_id { Some(p) => db.issue(p)?, None => None };
    let reviews = crate::reviews::for_issue(db, issue.id)?;
    let sessions = crate::ai::sessions_for_issue(db, issue.id)?;
    Ok(json!({
        "issue": issue,
        "events": db.issue_events(issue.id)?,
        "refs": db.issue_refs(issue.id)?,
        "links": db.issue_links(issue.id)?,
        "children": children,
        "parent": parent,
        "reviews": reviews,
        "sessions": sessions,
        "branch_name": branch_name(&issue),
    }))
}

async fn detail(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    let issue = issue_for(&s, &actor, id, Role::Read).await?;
    Ok(Json(detail_json(&s, issue).await?))
}

async fn by_number(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, number)): Path<(i64, i64)>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    let issue = s.engine.db.issue_by_number(id, number)?.ok_or_else(not_found)?;
    Ok(Json(detail_json(&s, issue).await?))
}

async fn update(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(f): Json<IssueFields>,
) -> ApiResult<impl IntoResponse> {
    let issue = issue_for(&s, &actor, id, Role::Write).await?;
    let changed = apply_fields(&s, &actor, &issue, &f)?;
    let issue = s.engine.db.issue(id)?.ok_or_else(not_found)?;
    if changed.contains(&"assignee_id") {
        crate::ai::on_assigned(&s.engine, &issue).await?;
    }
    Ok(Json(issue))
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
}

async fn comment(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(c): Json<CommentBody>,
) -> ApiResult<impl IntoResponse> {
    let issue = issue_for(&s, &actor, id, Role::Write).await?;
    if c.body.trim().is_empty() {
        return Err(bad_request("comment is empty"));
    }
    let eid = s.engine.db.add_issue_event(issue.id, Some(actor.user.id), "comment", c.body.trim(), None)?;
    crate::ai::on_mention(&s.engine, &issue, None, &actor.user, c.body.trim()).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": eid }))))
}

#[derive(Deserialize)]
struct LinkBody {
    /// `MOL-12` or a number.
    to: String,
    kind: String,
}

async fn add_link(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(l): Json<LinkBody>,
) -> ApiResult<impl IntoResponse> {
    let issue = issue_for(&s, &actor, id, Role::Write).await?;
    if !LINK_KINDS.contains(&l.kind.as_str()) {
        return Err(bad_request(format!("kind must be one of {}", LINK_KINDS.join(", "))));
    }
    let project = s.engine.db.project(issue.project_id)?.ok_or_else(not_found)?;
    let n = l.to.trim().parse::<i64>().ok().or_else(|| find_keys(&l.to, &project.key).first().copied()).ok_or_else(|| bad_request("bad issue key"))?;
    let other = s.engine.db.issue_by_number(project.id, n)?.ok_or_else(|| bad_request("no such issue"))?;
    if other.id == issue.id {
        return Err(bad_request("an issue can't link to itself"));
    }
    s.engine.db.link_issues(issue.id, other.id, &l.kind)?;
    s.engine.db.add_issue_event(issue.id, Some(actor.user.id), "link", &format!("{} {}", l.kind, other.key), None)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_link(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, to, kind)): Path<(i64, i64, String)>,
) -> ApiResult<impl IntoResponse> {
    issue_for(&s, &actor, id, Role::Write).await?;
    if s.engine.db.unlink_issues(id, to, &kind)? || s.engine.db.unlink_issues(to, id, &kind)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

#[derive(Deserialize)]
struct BranchBody {
    repo_id: i64,
}

/// Create the issue's branch from the default branch, so work is linked from the first push.
async fn make_branch(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(b): Json<BranchBody>,
) -> ApiResult<impl IntoResponse> {
    let issue = issue_for(&s, &actor, id, Role::Write).await?;
    let repo = s.engine.db.repo(b.repo_id)?.filter(|r| r.project_id == issue.project_id).ok_or_else(not_found)?;
    let name = branch_name(&issue);
    let path = s.engine.git.ensure(repo.id, &repo.default_branch).await?;
    let sha = s.engine.git.resolve(&path, &repo.default_branch).await.map_err(|_| bad_request("the repo has no commits yet"))?;
    let r = s
        .engine
        .apply_ref_updates(
            &repo,
            Some(&actor),
            &[RefUpdate { name: format!("refs/heads/{name}"), old: crate::repos::zero(), new: sha }],
            crate::repos::Source::Push,
            true,
        )
        .await?;
    if let Some(Err(e)) = r.into_iter().next() {
        return Err(ApiError(StatusCode::CONFLICT, e));
    }
    Ok((StatusCode::CREATED, Json(json!({ "branch": name }))))
}

async fn list_cycles(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    Ok(Json(s.engine.db.cycles(id)?))
}

#[derive(Deserialize)]
struct NewCycle {
    name: String,
    starts_at: i64,
    ends_at: i64,
}

async fn create_cycle(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(c): Json<NewCycle>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Write)?;
    if c.name.trim().is_empty() || c.ends_at <= c.starts_at {
        return Err(bad_request("a cycle needs a name and must end after it starts"));
    }
    let cid = s.engine.db.create_cycle(id, c.name.trim(), c.starts_at, c.ends_at)?;
    Ok((StatusCode::CREATED, Json(json!({ "id": cid }))))
}

async fn burndown(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    let project: Option<i64> = s.engine.db.conn().query_row("SELECT project_id FROM cycles WHERE id = ?1", [id], |r| r.get(0)).optional()?;
    need(&s, &actor, project.ok_or_else(not_found)?, Role::Read)?;
    Ok(Json(s.engine.db.burndown(id)?))
}

#[derive(Deserialize)]
struct ImportBody {
    /// `jira-csv` (Jira's CSV export) or `github-json` (`gh issue list --state all --json number,title,body,state,labels`).
    format: String,
    data: String,
}

fn map_jira_status(s: &str) -> &'static str {
    match s.to_lowercase().as_str() {
        "done" | "closed" | "resolved" => "done",
        "in progress" => "in_progress",
        "in review" | "review" | "code review" => "in_review",
        "backlog" => "backlog",
        "cancelled" | "won't do" | "won't fix" => "cancelled",
        _ => "todo",
    }
}

/// Import issues from Jira or GitHub. Returns how many were created.
async fn import(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(b): Json<ImportBody>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    let mut created = 0;
    match b.format.as_str() {
        "jira-csv" => {
            let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(b.data.as_bytes());
            let headers = rdr.headers().map_err(|e| bad_request(e.to_string()))?.clone();
            let col = |name: &str| headers.iter().position(|h| h.eq_ignore_ascii_case(name));
            let (summary, desc, typ, status, prio) = (col("Summary"), col("Description"), col("Issue Type"), col("Status"), col("Priority"));
            let labels: Vec<usize> = headers.iter().enumerate().filter(|(_, h)| h.eq_ignore_ascii_case("Labels")).map(|(i, _)| i).collect();
            let summary = summary.ok_or_else(|| bad_request("CSV needs a Summary column"))?;
            for rec in rdr.records() {
                let rec = rec.map_err(|e| bad_request(e.to_string()))?;
                let get = |i: Option<usize>| i.and_then(|i| rec.get(i)).unwrap_or("").to_string();
                let title = get(Some(summary));
                if title.trim().is_empty() {
                    continue;
                }
                let kind = match get(typ).to_lowercase().as_str() {
                    "bug" => "bug",
                    "story" => "story",
                    "epic" => "epic",
                    _ => "task",
                };
                let priority = match get(prio).to_lowercase().as_str() {
                    "highest" | "blocker" | "critical" => "urgent",
                    "high" => "high",
                    "medium" => "medium",
                    "low" | "lowest" => "low",
                    _ => "none",
                };
                let f = IssueFields {
                    body: Some(get(desc)),
                    kind: Some(kind.into()),
                    status: Some(map_jira_status(&get(status)).into()),
                    priority: Some(priority.into()),
                    labels: Some(labels.iter().filter_map(|&i| rec.get(i)).filter(|l| !l.is_empty()).map(str::to_string).collect()),
                    ..Default::default()
                };
                s.engine.db.create_issue(id, Some(actor.user.id), title.trim(), &f)?;
                created += 1;
            }
        }
        "github-json" => {
            let items: Vec<Value> = serde_json::from_str(&b.data).map_err(|e| bad_request(e.to_string()))?;
            for it in items {
                let Some(title) = it["title"].as_str() else { continue };
                let labels: Vec<String> = it["labels"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|l| l["name"].as_str().or(l.as_str()).map(str::to_string)).collect())
                    .unwrap_or_default();
                let closed = it["state"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("closed"));
                let f = IssueFields {
                    body: Some(it["body"].as_str().unwrap_or("").to_string()),
                    kind: Some(if labels.iter().any(|l| l == "bug") { "bug" } else { "task" }.into()),
                    status: Some(if closed { "done" } else { "todo" }.into()),
                    labels: Some(labels),
                    ..Default::default()
                };
                let iid = s.engine.db.create_issue(id, Some(actor.user.id), title, &f)?;
                if let Some(n) = it["number"].as_i64() {
                    s.engine.db.add_issue_event(iid, None, "import", &format!("Imported from GitHub issue #{n}"), None)?;
                }
                created += 1;
            }
        }
        _ => return Err(bad_request("format must be jira-csv or github-json")),
    }
    Ok(Json(json!({ "created": created })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_keys() {
        assert_eq!(find_keys("MOL-42-fix-login", "MOL"), vec![42]);
        assert_eq!(find_keys("feature/mol-7_and MOL-8", "MOL"), vec![7, 8]);
        assert_eq!(find_keys("XMOL-1 MOL- MOL-x", "MOL"), Vec::<i64>::new());
        assert_eq!(closing_keys("Fix login\n\nFixes MOL-3, refs MOL-4", "MOL"), vec![3]);
        assert_eq!(closing_keys("closes: MOL-9", "MOL"), vec![9]);
        assert!(valid_key("MOL") && valid_key("A1") && !valid_key("mol") && !valid_key("1A"));
    }

    #[test]
    fn parses_filters() {
        let f = Filter::parse("status:open assignee:me label:backend login", "dani");
        assert_eq!(f.open, Some(true));
        assert_eq!(f.assignee.as_deref(), Some("dani"));
        assert_eq!(f.label, vec!["backend"]);
        assert_eq!(f.text, vec!["login"]);
    }

    #[test]
    fn issue_crud_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let pid = db.create_project("moloko", "").unwrap();
        let uid = db.create_user("dani", None, "human", true).unwrap();
        let a = db.create_issue(pid, Some(uid), "Fix login redirect", &IssueFields { labels: Some(vec!["web".into()]), ..Default::default() }).unwrap();
        let b = db.create_issue(pid, Some(uid), "Speed up tests", &IssueFields::default()).unwrap();
        assert_eq!(db.issue(b).unwrap().unwrap().key, "MOL-2");
        assert!(db.set_issue_field(a, Some(uid), "status", json!("done")).unwrap());
        assert!(!db.set_issue_field(a, Some(uid), "status", json!("done")).unwrap());
        assert!(db.issue(a).unwrap().unwrap().closed_at.is_some());
        db.set_issue_field(b, Some(uid), "assignee_id", json!(uid)).unwrap();
        let open = db.issues(pid, &Filter::parse("is:open assignee:me", "dani"), 50).unwrap();
        assert_eq!(open.iter().map(|i| i.id).collect::<Vec<_>>(), vec![b]);
        let by_label = db.issues(pid, &Filter::parse("label:web login", "dani"), 50).unwrap();
        assert_eq!(by_label.len(), 1);
        let events = db.issue_events(a).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["data"]["to"], "done");
    }
}
