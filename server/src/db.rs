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
    pub repo_url: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct Run {
    pub id: i64,
    pub project_id: i64,
    pub project_name: String,
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

pub struct Db(Mutex<Connection>);

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
"#;

const RUN_COLS: &str = "r.id, r.project_id, p.name, r.git_ref, r.sha, r.status, r.trigger, r.error, r.created_at, r.started_at, r.finished_at";

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
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
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

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap()
    }

    pub fn create_project(&self, name: &str, repo_url: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO projects (name, repo_url, created_at) VALUES (?1, ?2, ?3)",
            params![name, repo_url, now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let c = self.conn();
        let mut st =
            c.prepare("SELECT id, name, repo_url, created_at FROM projects ORDER BY name")?;
        let rows = st.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
                repo_url: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, id: i64) -> Result<Option<Project>> {
        let c = self.conn();
        Ok(c.query_row(
            "SELECT id, name, repo_url, created_at FROM projects WHERE id = ?1",
            [id],
            |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    repo_url: r.get(2)?,
                    created_at: r.get(3)?,
                })
            },
        )
        .optional()?)
    }

    pub fn create_run(&self, project_id: i64, git_ref: &str, trigger: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO runs (project_id, git_ref, status, trigger, created_at) VALUES (?1, ?2, 'queued', ?3, ?4)",
            params![project_id, git_ref, trigger, now_ms()],
        )?;
        Ok(c.last_insert_rowid())
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

    pub fn job_exists(&self, id: i64) -> Result<bool> {
        let c = self.conn();
        Ok(
            c.query_row("SELECT 1 FROM jobs WHERE id = ?1", [id], |_| Ok(()))
                .optional()?
                .is_some(),
        )
    }
}
