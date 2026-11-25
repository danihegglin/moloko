//! SQLite persistence for projects, runs and jobs.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
/// Terminal states; `queued` and `running` are only written as SQL literals.
pub enum Status {
    Success,
    Failed,
    Skipped,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Success => "success",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    /// Upstream URL of the project's first repo, kept for older clients. Empty for hosted repos.
    pub repo_url: String,
    pub created_at: i64,
    /// Issue key prefix, e.g. `MOL` for `MOL-42`.
    pub key: String,
}

#[derive(Debug, Serialize)]
pub struct Run {
    pub id: i64,
    pub project_id: i64,
    pub project_name: String,
    pub repo_id: Option<i64>,
    pub repo_name: Option<String>,
    pub git_ref: String,
    pub sha: Option<String>,
    pub status: String,
    pub trigger: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Job {
    pub id: i64,
    pub run_id: i64,
    pub name: String,
    pub needs: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: i64,
    pub name: String,
    pub labels: Vec<String>,
    pub created_at: i64,
    pub last_seen: Option<i64>,
}

fn map_agent(r: &rusqlite::Row) -> rusqlite::Result<Agent> {
    let labels: String = r.get(2)?;
    Ok(Agent {
        id: r.get(0)?,
        name: r.get(1)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        created_at: r.get(3)?,
        last_seen: r.get(4)?,
    })
}

pub struct Db(Mutex<Connection>);

/// Projects from before multi-repo support get a repo for their URL, and a key.
fn migrate_projects(c: &Connection) -> Result<()> {
    let mut st = c.prepare(
        "SELECT id, name, repo_url, key FROM projects p WHERE key = '' OR (repo_url != '' AND NOT EXISTS (SELECT 1 FROM repos WHERE project_id = p.id))",
    )?;
    let rows: Vec<(i64, String, String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, name, url, key) in rows {
        if key.is_empty() {
            c.execute("UPDATE projects SET key = ?2 WHERE id = ?1", params![id, default_key(&name)])?;
        }
        let has_repo: bool = c.query_row("SELECT EXISTS (SELECT 1 FROM repos WHERE project_id = ?1)", [id], |r| r.get(0))?;
        if !url.is_empty() && !has_repo {
            c.execute(
                "INSERT INTO repos (project_id, name, upstream_url, mirror_mode, created_at) VALUES (?1, ?2, ?3, 'pull', ?4)",
                params![id, crate::repos::slug(&name), url, now_ms()],
            )?;
            c.execute("UPDATE runs SET repo_id = (SELECT id FROM repos WHERE project_id = ?1) WHERE project_id = ?1 AND repo_id IS NULL", [id])?;
        }
    }
    Ok(())
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    repo_url TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    git_ref TEXT NOT NULL,
    sha TEXT,
    status TEXT NOT NULL,
    trigger TEXT NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS runs_project ON runs(project_id, id DESC);
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs(id),
    name TEXT NOT NULL,
    needs TEXT NOT NULL,
    status TEXT NOT NULL,
    exit_code INTEGER,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_run ON jobs(run_id);
CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    labels TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_seen INTEGER
);
"#;

const RUN_COLS: &str = "r.id, r.project_id, p.name, r.git_ref, r.sha, r.status, r.trigger, r.error, r.created_at, r.started_at, r.finished_at, r.repo_id, (SELECT name FROM repos WHERE id = r.repo_id)";

fn map_run(r: &rusqlite::Row) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        project_id: r.get(1)?,
        project_name: r.get(2)?,
        git_ref: r.get(3)?,
        sha: r.get(4)?,
        status: r.get(5)?,
        trigger: r.get(6)?,
        error: r.get(7)?,
        created_at: r.get(8)?,
        started_at: r.get(9)?,
        finished_at: r.get(10)?,
        repo_id: r.get(11)?,
        repo_name: r.get(12)?,
    })
}

const PROJECT_COLS: &str = "id, name, repo_url, created_at, key";

fn map_project(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        repo_url: r.get(2)?,
        created_at: r.get(3)?,
        key: r.get(4)?,
    })
}

/// `moloko-api` -> `MA`, `moloko` -> `MOL`: uppercase letters for issue keys.
pub fn default_key(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let key: String = if words.len() > 1 {
        words.iter().filter_map(|w| w.chars().next()).collect()
    } else {
        name.chars().filter(|c| c.is_ascii_alphabetic()).take(3).collect()
    };
    let key = key.to_ascii_uppercase();
    if key.is_empty() { "PRJ".into() } else { key }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        // Added after the first release; ignore the error when the column exists.
        let _ = conn.execute("ALTER TABLE agents ADD COLUMN wg_public TEXT", []);
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN repo_id INTEGER", []);
        let _ = conn.execute("ALTER TABLE projects ADD COLUMN key TEXT NOT NULL DEFAULT ''", []);
        for schema in [
            crate::auth::SCHEMA,
            crate::repos::SCHEMA,
            crate::issues::SCHEMA,
            crate::reviews::SCHEMA,
            crate::ai::SCHEMA,
        ] {
            conn.execute_batch(schema)?;
        }
        migrate_projects(&conn)?;
        // Anything still in flight belonged to a previous process.
        let now = now_ms();
        conn.execute(
            "UPDATE jobs SET status = 'cancelled', finished_at = ?1 WHERE status IN ('queued', 'running')",
            [now],
        )?;
        conn.execute(
            "UPDATE runs SET status = 'cancelled', finished_at = ?1, error = 'server restarted' WHERE status IN ('queued', 'running')",
            [now],
        )?;
        Ok(Db(Mutex::new(conn)))
    }

    pub(crate) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap()
    }

    /// Create a project. With a `repo_url`, it also gets a first repo that pulls from that URL.
    pub fn create_project(&self, name: &str, repo_url: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO projects (name, repo_url, created_at, key) VALUES (?1, ?2, ?3, ?4)",
            params![name, repo_url, now_ms(), default_key(name)],
        )?;
        let id = c.last_insert_rowid();
        if !repo_url.is_empty() {
            c.execute(
                "INSERT INTO repos (project_id, name, upstream_url, mirror_mode, created_at) VALUES (?1, ?2, ?3, 'pull', ?4)",
                params![id, crate::repos::slug(name), repo_url, now_ms()],
            )?;
        }
        Ok(id)
    }

    pub fn set_project_key(&self, id: i64, key: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE projects SET key = ?2 WHERE id = ?1", params![id, key])?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {PROJECT_COLS} FROM projects ORDER BY name"))?;
        let rows = st.query_map([], map_project)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, id: i64) -> Result<Option<Project>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"),
            [id],
            map_project,
        )
        .optional()?)
    }

    pub fn project_by_name(&self, name: &str) -> Result<Option<Project>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {PROJECT_COLS} FROM projects WHERE name = ?1 COLLATE NOCASE"),
            [name],
            map_project,
        )
        .optional()?)
    }

    pub fn create_run(
        &self,
        project_id: i64,
        repo_id: i64,
        git_ref: &str,
        trigger: &str,
    ) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO runs (project_id, repo_id, git_ref, status, trigger, created_at) VALUES (?1, ?2, ?3, 'queued', ?4, ?5)",
            params![project_id, repo_id, git_ref, trigger, now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Latest run of `repo_id` at `sha`, if any.
    pub fn run_for_sha(&self, repo_id: i64, sha: &str) -> Result<Option<Run>> {
        let c = self.conn();
        let sql = format!(
            "SELECT {RUN_COLS} FROM runs r JOIN projects p ON p.id = r.project_id
             WHERE r.repo_id = ?1 AND r.sha = ?2 ORDER BY r.id DESC LIMIT 1"
        );
        Ok(c.query_row(&sql, params![repo_id, sha], map_run).optional()?)
    }

    pub fn start_run(&self, id: i64, sha: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE runs SET status = 'running', sha = ?2, started_at = ?3 WHERE id = ?1",
            params![id, sha, now_ms()],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, id: i64, status: Status, error: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE runs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
            params![id, status.as_str(), error, now_ms()],
        )?;
        Ok(())
    }

    pub fn runs(&self, project_id: Option<i64>, limit: i64) -> Result<Vec<Run>> {
        let c = self.conn();
        let sql = format!(
            "SELECT {RUN_COLS} FROM runs r JOIN projects p ON p.id = r.project_id
             WHERE (?1 IS NULL OR r.project_id = ?1) ORDER BY r.id DESC LIMIT ?2"
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![project_id, limit], map_run)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn run(&self, id: i64) -> Result<Option<Run>> {
        let c = self.conn();
        let sql = format!(
            "SELECT {RUN_COLS} FROM runs r JOIN projects p ON p.id = r.project_id WHERE r.id = ?1"
        );
        Ok(c.query_row(&sql, [id], map_run).optional()?)
    }

    pub fn create_job(&self, run_id: i64, name: &str, needs: &[String]) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO jobs (run_id, name, needs, status) VALUES (?1, ?2, ?3, 'queued')",
            params![run_id, name, serde_json::to_string(needs)?],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn start_job(&self, id: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE jobs SET status = 'running', started_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn finish_job(&self, id: i64, status: Status, exit_code: Option<i32>) -> Result<()> {
        self.conn().execute(
            "UPDATE jobs SET status = ?2, exit_code = ?3, finished_at = ?4 WHERE id = ?1",
            params![id, status.as_str(), exit_code, now_ms()],
        )?;
        Ok(())
    }

    pub fn jobs(&self, run_id: i64) -> Result<Vec<Job>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT id, run_id, name, needs, status, exit_code, started_at, finished_at FROM jobs WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([run_id], |r| {
            let needs: String = r.get(3)?;
            Ok(Job {
                id: r.get(0)?,
                run_id: r.get(1)?,
                name: r.get(2)?,
                needs: serde_json::from_str(&needs).unwrap_or_default(),
                status: r.get(4)?,
                exit_code: r.get(5)?,
                started_at: r.get(6)?,
                finished_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn job_project(&self, id: i64) -> Result<Option<i64>> {
        let c = self.conn();
        Ok(c.query_row(
            "SELECT r.project_id FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
    }

    pub fn create_agent(&self, name: &str, labels: &[String], token_hash: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO agents (name, labels, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, serde_json::to_string(labels)?, token_hash, now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn set_agent_token_hash(&self, id: i64, token_hash: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET token_hash = ?2 WHERE id = ?1",
            params![id, token_hash],
        )?;
        Ok(())
    }

    pub fn set_agent_wg_public(&self, id: i64, public_b64: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET wg_public = ?2 WHERE id = ?1",
            params![id, public_b64],
        )?;
        Ok(())
    }

    /// (agent id, WireGuard public key) for every agent that has one.
    pub fn agent_wg_keys(&self) -> Result<Vec<(i64, String)>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, wg_public FROM agents WHERE wg_public IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        let c = self.conn();
        let mut st =
            c.prepare("SELECT id, name, labels, created_at, last_seen FROM agents ORDER BY name")?;
        let rows = st.query_map([], map_agent)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn agent_by_token(&self, token_hash: &str) -> Result<Option<Agent>> {
        let c = self.conn();
        Ok(c.query_row(
            "SELECT id, name, labels, created_at, last_seen FROM agents WHERE token_hash = ?1",
            [token_hash],
            map_agent,
        )
        .optional()?)
    }

    pub fn touch_agent(&self, id: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET last_seen = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_agent(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM agents WHERE id = ?1", [id])?
            > 0)
    }
}
