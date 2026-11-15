//! SQLite persistence for projects, runs and jobs.

use anyhow::{Result, bail};
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
    pub created_at: i64,
    pub repos: Vec<Repo>,
}

/// A git repository inside a project. Each repo has its own mirror, pipeline and runs.
#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub id: i64,
    pub project_id: i64,
    pub project_name: String,
    pub name: String,
    pub url: String,
    pub created_at: i64,
    /// Check the remote for new commits and start push runs for them.
    pub poll: bool,
}

#[derive(Debug, Serialize)]
pub struct Run {
    pub id: i64,
    pub repo_id: i64,
    pub repo_name: String,
    pub project_id: i64,
    pub project_name: String,
    pub git_ref: String,
    pub sha: Option<String>,
    pub status: String,
    pub trigger: String,
    /// Runs started by the same trigger (one per pipeline) share this: the id of the first of them.
    pub group_id: i64,
    /// `.moloko.yml` or the GitHub workflow's name.
    pub workflow: Option<String>,
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
    /// The step a running job is on. Not stored: the API fills it in from the live log.
    pub step: Option<String>,
}

/// (name, nonce, sealed value)
pub type SealedSecret = (String, Vec<u8>, Vec<u8>);

#[derive(Debug, Serialize)]
pub struct SecretInfo {
    pub name: String,
    pub updated_at: i64,
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

pub struct Db(
    Mutex<Connection>,
    /// Jobs that were running when the previous process stopped.
    pub Vec<i64>,
);

const TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (project_id, name)
);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    git_ref TEXT NOT NULL,
    sha TEXT,
    status TEXT NOT NULL,
    trigger TEXT NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS runs_repo ON runs(repo_id, id DESC);
-- The remote's branches and tags as last seen, for repos that are polled.
CREATE TABLE IF NOT EXISTS repo_refs (
    repo_id INTEGER NOT NULL,
    ref TEXT NOT NULL,
    sha TEXT NOT NULL,
    PRIMARY KEY (repo_id, ref)
);
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
CREATE TABLE IF NOT EXISTS secrets (
    scope TEXT NOT NULL,
    name TEXT NOT NULL,
    nonce BLOB NOT NULL,
    value BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (scope, name)
);
CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    labels TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_seen INTEGER
);
"#;

/// Before projects held repos, each project had one `repo_url` and runs pointed at the project.
/// Turn every old project into a project with one repo of the same id, so git mirrors
/// (`git/<id>.git`) and run history stay valid.
const MIGRATE_REPOS: &str = r#"
ALTER TABLE projects RENAME TO projects_old;
ALTER TABLE runs RENAME TO runs_old;
"#;
const MIGRATE_REPOS_COPY: &str = r#"
INSERT INTO projects (id, name, created_at) SELECT id, name, created_at FROM projects_old;
INSERT INTO repos (id, project_id, name, url, created_at)
    SELECT id, id, name, repo_url, created_at FROM projects_old;
INSERT INTO runs (id, repo_id, git_ref, sha, status, trigger, error, created_at, started_at, finished_at)
    SELECT id, project_id, git_ref, sha, status, trigger, error, created_at, started_at, finished_at FROM runs_old;
DROP TABLE runs_old;
DROP TABLE projects_old;
"#;

const RUN_COLS: &str = "r.id, r.repo_id, g.name, g.project_id, p.name, r.git_ref, r.sha, r.status, r.trigger, r.error, r.created_at, r.started_at, r.finished_at, r.workflow, COALESCE(r.group_id, r.id)";
const RUN_FROM: &str =
    "runs r JOIN repos g ON g.id = r.repo_id JOIN projects p ON p.id = g.project_id";

fn map_run(r: &rusqlite::Row) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        repo_name: r.get(2)?,
        project_id: r.get(3)?,
        project_name: r.get(4)?,
        git_ref: r.get(5)?,
        sha: r.get(6)?,
        status: r.get(7)?,
        trigger: r.get(8)?,
        error: r.get(9)?,
        created_at: r.get(10)?,
        started_at: r.get(11)?,
        finished_at: r.get(12)?,
        workflow: r.get(13)?,
        group_id: r.get(14)?,
    })
}

const REPO_COLS: &str = "g.id, g.project_id, p.name, g.name, g.url, g.created_at, g.poll";

fn map_repo(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: r.get(0)?,
        project_id: r.get(1)?,
        project_name: r.get(2)?,
        name: r.get(3)?,
        url: r.get(4)?,
        created_at: r.get(5)?,
        poll: r.get(6)?,
    })
}

const ACTIVE_RUN_OF_REPOS: &str =
    "SELECT 1 FROM runs WHERE status IN ('queued', 'running') AND repo_id IN";

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        let has_repo_url: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('projects') WHERE name = 'repo_url'")?
            .exists([])?;
        if has_repo_url {
            // foreign_keys can't change inside a transaction; legacy_alter_table stops the
            // renames from rewriting the `jobs -> runs` reference to point at `runs_old`.
            conn.execute_batch(&format!(
                "PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON; BEGIN;
                 {MIGRATE_REPOS} {TABLES} {MIGRATE_REPOS_COPY}
                 COMMIT; PRAGMA legacy_alter_table = OFF;"
            ))?;
        }
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(TABLES)?;
        // Added after the first release; ignore the error when the column exists.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN workflow TEXT", []);
        // Added after the first release; ignore the error when the column exists.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN group_id INTEGER", []);
        // Added after the first release; ignore the errors when the columns exist.
        // `polled_at` is NULL until the first poll has recorded the remote's refs.
        let _ = conn.execute("ALTER TABLE repos ADD COLUMN poll INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE repos ADD COLUMN polled_at INTEGER", []);
        // Added after the first release; ignore the error when the column exists.
        let _ = conn.execute("ALTER TABLE agents ADD COLUMN wg_public TEXT", []);
        // Anything still in flight belonged to a previous process.
        let interrupted = conn
            .prepare("SELECT id FROM jobs WHERE status = 'running'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        let now = now_ms();
        conn.execute(
            "UPDATE jobs SET status = 'cancelled', finished_at = ?1 WHERE status IN ('queued', 'running')",
            [now],
        )?;
        conn.execute(
            "UPDATE runs SET status = 'cancelled', finished_at = ?1, error = 'server restarted' WHERE status IN ('queued', 'running')",
            [now],
        )?;
        Ok(Db(Mutex::new(conn), interrupted))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap()
    }

    pub fn create_project(&self, name: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO projects (name, created_at) VALUES (?1, ?2)",
            params![name, now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn rename_project(&self, id: i64, name: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE projects SET name = ?2 WHERE id = ?1",
            params![id, name],
        )? > 0)
    }

    /// Delete a project with its repos, runs and jobs. Returns the deleted repo ids,
    /// `None` if the project doesn't exist. Fails while any of its runs is active.
    pub fn delete_project(&self, id: i64) -> Result<Option<Vec<i64>>> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        if !tx
            .prepare("SELECT 1 FROM projects WHERE id = ?1")?
            .exists([id])?
        {
            return Ok(None);
        }
        let repos: Vec<i64> = tx
            .prepare("SELECT id FROM repos WHERE project_id = ?1")?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let sub = "(SELECT id FROM repos WHERE project_id = ?1)";
        if tx
            .prepare(&format!("{ACTIVE_RUN_OF_REPOS} {sub}"))?
            .exists([id])?
        {
            bail!("a run of this project is still active; cancel it first");
        }
        tx.execute(
            &format!("DELETE FROM jobs WHERE run_id IN (SELECT id FROM runs WHERE repo_id IN {sub})"),
            [id],
        )?;
        tx.execute(&format!("DELETE FROM runs WHERE repo_id IN {sub}"), [id])?;
        tx.execute(
            "DELETE FROM secrets WHERE scope = 'p:' || ?1 OR scope IN (SELECT 'r:' || id FROM repos WHERE project_id = ?1)",
            [id],
        )?;
        tx.execute(&format!("DELETE FROM repo_refs WHERE repo_id IN {sub}"), [id])?;
        tx.execute("DELETE FROM repos WHERE project_id = ?1", [id])?;
        tx.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Some(repos))
    }

    /// All projects with their repos.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut repos = self.repos()?;
        let c = self.conn();
        let mut st = c.prepare("SELECT id, name, created_at FROM projects ORDER BY name")?;
        let rows = st.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                repos: Vec::new(),
            })
        })?;
        let mut projects: Vec<Project> = rows.collect::<rusqlite::Result<_>>()?;
        for p in &mut projects {
            p.repos = repos.extract_if(.., |r| r.project_id == p.id).collect();
        }
        Ok(projects)
    }

    pub fn project_exists(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .prepare("SELECT 1 FROM projects WHERE id = ?1")?
            .exists([id])?)
    }

    pub fn create_repo(&self, project_id: i64, name: &str, url: &str, poll: bool) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO repos (project_id, name, url, created_at, poll) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, name, url, now_ms(), poll],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn repos(&self) -> Result<Vec<Repo>> {
        let c = self.conn();
        let mut st = c.prepare(&format!(
            "SELECT {REPO_COLS} FROM repos g JOIN projects p ON p.id = g.project_id ORDER BY g.name"
        ))?;
        let rows = st.query_map([], map_repo)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn repo(&self, id: i64) -> Result<Option<Repo>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!(
                "SELECT {REPO_COLS} FROM repos g JOIN projects p ON p.id = g.project_id WHERE g.id = ?1"
            ),
            [id],
            map_repo,
        )
        .optional()?)
    }

    /// Change a repo's name and/or URL. Changing the URL fails while a run of the repo is active.
    pub fn update_repo(&self, id: i64, name: Option<&str>, url: Option<&str>, poll: Option<bool>) -> Result<bool> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        if url.is_some() && tx.prepare(&format!("{ACTIVE_RUN_OF_REPOS} (?1)"))?.exists([id])? {
            bail!("a run of this repo is still active; cancel it first");
        }
        let n = tx.execute(
            "UPDATE repos SET name = COALESCE(?2, name), url = COALESCE(?3, url), poll = COALESCE(?4, poll) WHERE id = ?1",
            params![id, name, url, poll],
        )?;
        if url.is_some() || poll.is_some() {
            // What was seen before no longer applies: the next poll only records the refs.
            tx.execute("UPDATE repos SET polled_at = NULL WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM repo_refs WHERE repo_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Delete a repo with its runs and jobs. Fails while one of its runs is active.
    pub fn delete_repo(&self, id: i64) -> Result<bool> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        if tx.prepare(&format!("{ACTIVE_RUN_OF_REPOS} (?1)"))?.exists([id])? {
            bail!("a run of this repo is still active; cancel it first");
        }
        tx.execute(
            "DELETE FROM jobs WHERE run_id IN (SELECT id FROM runs WHERE repo_id = ?1)",
            [id],
        )?;
        tx.execute("DELETE FROM runs WHERE repo_id = ?1", [id])?;
        tx.execute("DELETE FROM secrets WHERE scope = 'r:' || ?1", [id])?;
        tx.execute("DELETE FROM repo_refs WHERE repo_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM repos WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// `group` is the first run of the same trigger; `None` starts a new group.
    pub fn create_run(&self, repo_id: i64, git_ref: &str, trigger: &str, group: Option<i64>) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO runs (repo_id, git_ref, status, trigger, created_at, group_id) VALUES (?1, ?2, 'queued', ?3, ?4, ?5)",
            params![repo_id, git_ref, trigger, now_ms(), group],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// The runs one trigger started, oldest first.
    pub fn run_group(&self, group_id: i64) -> Result<Vec<Run>> {
        let c = self.conn();
        let sql = format!("SELECT {RUN_COLS} FROM {RUN_FROM} WHERE COALESCE(r.group_id, r.id) = ?1 ORDER BY r.id");
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map([group_id], map_run)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_run_workflow(&self, id: i64, workflow: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE runs SET workflow = ?2 WHERE id = ?1",
            params![id, workflow],
        )?;
        Ok(())
    }

    pub fn rename_job(&self, id: i64, name: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE jobs SET name = ?2 WHERE id = ?1", params![id, name])?;
        Ok(())
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

    pub fn runs(
        &self,
        project_id: Option<i64>,
        repo_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Run>> {
        let c = self.conn();
        let sql = format!(
            "SELECT {RUN_COLS} FROM {RUN_FROM}
             WHERE (?1 IS NULL OR g.project_id = ?1) AND (?2 IS NULL OR r.repo_id = ?2)
             ORDER BY r.id DESC LIMIT ?3"
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![project_id, repo_id, limit], map_run)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn run(&self, id: i64) -> Result<Option<Run>> {
        let c = self.conn();
        let sql = format!("SELECT {RUN_COLS} FROM {RUN_FROM} WHERE r.id = ?1");
        Ok(c.query_row(&sql, [id], map_run).optional()?)
    }

    /// Remove a run that turned out to have nothing to do.
    pub fn delete_run(&self, id: i64) -> Result<()> {
        let c = self.conn();
        c.execute("DELETE FROM jobs WHERE run_id = ?1", [id])?;
        c.execute("DELETE FROM runs WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The remote refs recorded by the last poll, or `None` before the first one.
    pub fn repo_refs(&self, repo_id: i64) -> Result<Option<std::collections::HashMap<String, String>>> {
        let c = self.conn();
        let polled: Option<i64> = c.query_row("SELECT polled_at FROM repos WHERE id = ?1", [repo_id], |r| r.get(0))?;
        if polled.is_none() {
            return Ok(None);
        }
        let mut st = c.prepare("SELECT ref, sha FROM repo_refs WHERE repo_id = ?1")?;
        let rows = st.query_map([repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(Some(rows.collect::<rusqlite::Result<_>>()?))
    }

    /// Record what a poll saw, replacing the previous record.
    pub fn set_repo_refs(&self, repo_id: i64, refs: &[(String, String)]) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM repo_refs WHERE repo_id = ?1", [repo_id])?;
        for (r, sha) in refs {
            tx.execute("INSERT OR REPLACE INTO repo_refs (repo_id, ref, sha) VALUES (?1, ?2, ?3)", params![repo_id, r, sha])?;
        }
        tx.execute("UPDATE repos SET polled_at = ?2 WHERE id = ?1", params![repo_id, now_ms()])?;
        tx.commit()?;
        Ok(())
    }

    /// Record that `git_ref` is at `sha`. Returns false when that was already known,
    /// so a push reported by both a webhook and the poller starts only one run.
    pub fn note_ref(&self, repo_id: i64, git_ref: &str, sha: &str) -> Result<bool> {
        let c = self.conn();
        let known: Option<String> = c
            .query_row("SELECT sha FROM repo_refs WHERE repo_id = ?1 AND ref = ?2", params![repo_id, git_ref], |r| r.get(0))
            .optional()?;
        if known.as_deref() == Some(sha) {
            return Ok(false);
        }
        c.execute("INSERT OR REPLACE INTO repo_refs (repo_id, ref, sha) VALUES (?1, ?2, ?3)", params![repo_id, git_ref, sha])?;
        Ok(true)
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
                step: None,
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

    pub fn put_secret(&self, scope: &str, name: &str, nonce: &[u8], value: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT INTO secrets (scope, name, nonce, value, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (scope, name) DO UPDATE SET nonce = ?3, value = ?4, updated_at = ?5",
            params![scope, name, nonce, value, now_ms()],
        )?;
        Ok(())
    }

    /// Names and update times only; values never leave the database encrypted or not.
    pub fn secret_names(&self, scope: &str) -> Result<Vec<SecretInfo>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT name, updated_at FROM secrets WHERE scope = ?1 ORDER BY name")?;
        let rows = st.query_map([scope], |r| Ok(SecretInfo { name: r.get(0)?, updated_at: r.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_secret(&self, scope: &str, name: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM secrets WHERE scope = ?1 AND name = ?2", params![scope, name])?
            > 0)
    }

    /// Every secret in `scope`, still encrypted.
    pub fn sealed_secrets(&self, scope: &str) -> Result<Vec<SealedSecret>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT name, nonce, value FROM secrets WHERE scope = ?1")?;
        let rows = st.query_map([scope], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_projects_with_repo_url_to_repos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moloko.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, repo_url TEXT NOT NULL, created_at INTEGER NOT NULL);
                 CREATE TABLE runs (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id), git_ref TEXT NOT NULL, sha TEXT, status TEXT NOT NULL, trigger TEXT NOT NULL, error TEXT, created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER);
                 CREATE INDEX runs_project ON runs(project_id, id DESC);
                 CREATE TABLE jobs (id INTEGER PRIMARY KEY, run_id INTEGER NOT NULL REFERENCES runs(id), name TEXT NOT NULL, needs TEXT NOT NULL, status TEXT NOT NULL, exit_code INTEGER, started_at INTEGER, finished_at INTEGER);
                 INSERT INTO projects VALUES (7, 'shop', 'https://github.com/acme/shop', 1);
                 INSERT INTO runs VALUES (3, 7, 'main', 'abc', 'success', 'manual', NULL, 2, 2, 3);
                 INSERT INTO jobs VALUES (5, 3, 'build', '[]', 'success', 0, 2, 3);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let projects = db.projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "shop");
        let repo = &projects[0].repos[0];
        assert_eq!((repo.id, repo.name.as_str()), (7, "shop"));
        assert_eq!(repo.url, "https://github.com/acme/shop");
        let run = db.run(3).unwrap().unwrap();
        assert_eq!((run.repo_id, run.project_name.as_str()), (7, "shop"));
        assert_eq!(db.jobs(3).unwrap()[0].name, "build");
        // jobs still reference the new runs table, so deleting cascades cleanly.
        assert_eq!(db.delete_project(7).unwrap(), Some(vec![7]));
        assert!(!db.job_exists(5).unwrap());
        // Opening again is a no-op.
        drop(db);
        Db::open(&path).unwrap();
    }

    #[test]
    fn update_and_delete_refuse_active_runs() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("moloko.db")).unwrap();
        let p = db.create_project("shop").unwrap();
        let api = db.create_repo(p, "api", "https://x/api", false).unwrap();
        let web = db.create_repo(p, "web", "https://x/web", false).unwrap();
        assert!(db.create_repo(p, "api", "https://x/other", false).is_err());
        let run = db.create_run(api, "main", "manual", None).unwrap();
        db.create_job(run, "build", &[]).unwrap();

        assert!(db.update_repo(api, None, Some("git@x:api.git"), None).is_err());
        assert!(db.update_repo(api, Some("backend"), None, None).unwrap());
        assert!(db.delete_repo(api).is_err());
        assert!(db.delete_project(p).is_err());

        db.finish_run(run, Status::Success, None).unwrap();
        assert!(db.update_repo(api, None, Some("git@x:api.git"), None).unwrap());
        assert_eq!(db.repo(api).unwrap().unwrap().url, "git@x:api.git");
        assert_eq!(db.repo(api).unwrap().unwrap().name, "backend");
        assert!(db.delete_repo(web).unwrap());
        assert!(!db.delete_repo(web).unwrap());
        assert_eq!(db.delete_project(p).unwrap(), Some(vec![api]));
        assert!(db.runs(None, None, 10).unwrap().is_empty());
        assert_eq!(db.delete_project(p).unwrap(), None);
    }
}
