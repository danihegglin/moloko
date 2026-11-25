//! AI agents: teammates you assign issues to. Each session runs a command (for example Claude
//! Code in headless mode) in a fresh container or VM with the project's repos checked out, then
//! moloko commits what changed, pushes the issue's branch and opens a review. Agents answer
//! review comments and can fix failing checks. They never merge.

use crate::api::{ApiResult, AppState, bad_request, need, not_found};
use crate::auth::{Actor, Role, Scope, User};
use crate::db::{Db, Status, now_ms};
use crate::engine::{Engine, RunCtx};
use crate::executor::{Executor, StepResult};
use crate::issues::Issue;
use crate::repos::{RefUpdate, Repo, Source};
use crate::reviews::Review;
use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch, post, put};
use axum::{Extension, Json, Router};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ai_agents (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    environment TEXT NOT NULL,
    command TEXT NOT NULL,
    review_command TEXT,
    secrets TEXT NOT NULL DEFAULT '[]',
    timeout_secs INTEGER NOT NULL DEFAULT 2700,
    max_sessions INTEGER NOT NULL DEFAULT 1,
    auto_fix_ci INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS project_secrets (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (project_id, name)
);
CREATE TABLE IF NOT EXISTS ai_sessions (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES ai_agents(id) ON DELETE CASCADE,
    issue_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
    review_id INTEGER REFERENCES reviews(id) ON DELETE SET NULL,
    run_id INTEGER,
    kind TEXT NOT NULL,
    instruction TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    branch TEXT,
    result TEXT,
    created_at INTEGER NOT NULL,
    finished_at INTEGER
);
"#;

/// Where a session runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    /// `container`, `vm`, or `host` (no isolation: only for agents you fully trust).
    pub kind: String,
    /// Container image, or VM base image (e.g. a tart image on macOS).
    pub image: String,
    pub cpus: Option<u32>,
    /// e.g. `8G`.
    pub memory: Option<String>,
    /// VM driver: `tart` (macOS) or a path to a driver executable (see `docs/vm-driver.md`).
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiAgent {
    pub id: i64,
    pub project_id: i64,
    pub user_id: i64,
    pub name: String,
    pub environment: Environment,
    pub command: String,
    pub review_command: Option<String>,
    pub secrets: Vec<String>,
    pub timeout_secs: i64,
    pub max_sessions: i64,
    pub auto_fix_ci: bool,
    pub created_at: i64,
}

const AGENT_COLS: &str = "a.id, a.project_id, a.user_id, u.username, a.environment, a.command, a.review_command, a.secrets, a.timeout_secs, a.max_sessions, a.auto_fix_ci, a.created_at";

fn map_agent(r: &rusqlite::Row) -> rusqlite::Result<AiAgent> {
    let env: String = r.get(4)?;
    let secrets: String = r.get(7)?;
    Ok(AiAgent {
        id: r.get(0)?,
        project_id: r.get(1)?,
        user_id: r.get(2)?,
        name: r.get(3)?,
        environment: serde_json::from_str(&env).unwrap_or(Environment { kind: "container".into(), image: String::new(), cpus: None, memory: None, driver: None }),
        command: r.get(5)?,
        review_command: r.get(6)?,
        secrets: serde_json::from_str(&secrets).unwrap_or_default(),
        timeout_secs: r.get(8)?,
        max_sessions: r.get(9)?,
        auto_fix_ci: r.get::<_, i64>(10)? != 0,
        created_at: r.get(11)?,
    })
}

/// Limits concurrent sessions per agent.
#[derive(Default)]
pub struct AiHub {
    slots: Mutex<HashMap<i64, Arc<Semaphore>>>,
}

impl AiHub {
    fn slot(&self, agent: &AiAgent) -> Arc<Semaphore> {
        self.slots
            .lock()
            .unwrap()
            .entry(agent.id)
            .or_insert_with(|| Arc::new(Semaphore::new(agent.max_sessions.max(1) as usize)))
            .clone()
    }
}

impl Db {
    pub fn ai_agents(&self, project_id: i64) -> Result<Vec<AiAgent>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {AGENT_COLS} FROM ai_agents a JOIN users u ON u.id = a.user_id WHERE a.project_id = ?1 ORDER BY u.username"))?;
        let rows = st.query_map([project_id], map_agent)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn ai_agent(&self, id: i64) -> Result<Option<AiAgent>> {
        let c = self.conn();
        Ok(c.query_row(&format!("SELECT {AGENT_COLS} FROM ai_agents a JOIN users u ON u.id = a.user_id WHERE a.id = ?1"), [id], map_agent).optional()?)
    }

    pub fn ai_agent_for_user(&self, project_id: i64, user_id: i64) -> Result<Option<AiAgent>> {
        Ok(self.ai_agents(project_id)?.into_iter().find(|a| a.user_id == user_id))
    }

    pub fn set_secret(&self, project_id: i64, name: &str, sealed: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO project_secrets (project_id, name, value) VALUES (?1, ?2, ?3) ON CONFLICT (project_id, name) DO UPDATE SET value = excluded.value",
            params![project_id, name, sealed],
        )?;
        Ok(())
    }

    pub fn secret(&self, project_id: i64, name: &str) -> Result<Option<String>> {
        Ok(self.conn().query_row("SELECT value FROM project_secrets WHERE project_id = ?1 AND name = ?2", params![project_id, name], |r| r.get(0)).optional()?)
    }

    pub fn secret_names(&self, project_id: i64) -> Result<Vec<String>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT name FROM project_secrets WHERE project_id = ?1 ORDER BY name")?;
        let rows = st.query_map([project_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn create_ai_session(&self, agent: i64, issue: Option<i64>, review: Option<i64>, kind: &str, instruction: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO ai_sessions (agent_id, issue_id, review_id, kind, instruction, state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
            params![agent, issue, review, kind, instruction, now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    fn sessions_where(&self, cond: &str, args: impl rusqlite::Params) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare(&format!(
            "SELECT s.id, u.username, s.issue_id, s.review_id, s.run_id, s.kind, s.instruction, s.state, s.branch, s.result, s.created_at, s.finished_at
             FROM ai_sessions s JOIN ai_agents a ON a.id = s.agent_id JOIN users u ON u.id = a.user_id WHERE {cond} ORDER BY s.id DESC LIMIT 200"
        ))?;
        let rows = st.query_map(args, |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "agent": r.get::<_, String>(1)?, "issue_id": r.get::<_, Option<i64>>(2)?,
                "review_id": r.get::<_, Option<i64>>(3)?, "run_id": r.get::<_, Option<i64>>(4)?, "kind": r.get::<_, String>(5)?,
                "instruction": r.get::<_, String>(6)?, "state": r.get::<_, String>(7)?, "branch": r.get::<_, Option<String>>(8)?,
                "result": r.get::<_, Option<String>>(9)?, "created_at": r.get::<_, i64>(10)?, "finished_at": r.get::<_, Option<i64>>(11)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn set_session(&self, id: i64, state: &str, result: Option<&str>) -> Result<()> {
        let finished = matches!(state, "success" | "failed" | "cancelled").then(now_ms);
        self.conn().execute(
            "UPDATE ai_sessions SET state = ?2, result = COALESCE(?3, result), finished_at = ?4 WHERE id = ?1",
            params![id, state, result, finished],
        )?;
        Ok(())
    }
}

pub fn sessions_for_issue(db: &Db, issue_id: i64) -> Result<Vec<Value>> {
    db.sessions_where("s.issue_id = ?1", [issue_id])
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What a session works on.
#[derive(Clone)]
enum Target {
    Issue(Issue),
    Review(Review),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Implement an issue, or follow up on it.
    Task,
    /// Review someone's change and approve or request changes.
    Review,
    /// Fix failing checks on the agent's own review.
    Fix,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Task => "task",
            Kind::Review => "review",
            Kind::Fix => "fix",
        }
    }
}

/// Queue a session; it starts when the agent has a free slot.
fn start(engine: &Arc<Engine>, agent: AiAgent, target: Target, kind: Kind, instruction: String) -> Result<i64> {
    let (issue_id, review_id) = match &target {
        Target::Issue(i) => (Some(i.id), None),
        Target::Review(r) => (r.issue_id, Some(r.id)),
    };
    let id = engine.db.create_ai_session(agent.id, issue_id, review_id, kind.as_str(), &instruction)?;
    if let Some(i) = issue_id {
        engine.db.add_issue_event(i, Some(agent.user_id), "ai", &format!("@{} started a {} session", agent.name, kind.as_str()), Some(json!({ "session": id })))?;
    }
    let engine = engine.clone();
    tokio::spawn(async move {
        let slot = engine.ai.slot(&agent);
        let _permit = slot.acquire_owned().await;
        if let Err(e) = run_session(&engine, &agent, id, target.clone(), kind, &instruction).await {
            tracing::warn!(session = id, "AI session failed: {e:#}");
            let _ = engine.db.set_session(id, "failed", Some(&format!("{e:#}")));
            if let Some(i) = issue_id {
                let _ = engine.db.add_issue_event(i, Some(agent.user_id), "ai", &format!("Session failed: {e:#}"), Some(json!({ "session": id })));
            }
        }
    });
    Ok(id)
}

struct Workspace {
    dir: PathBuf,
    primary: Repo,
    checkouts: Vec<(Repo, PathBuf, String)>,
}

fn task_text(engine: &Engine, target: &Target, kind: Kind, instruction: &str, branch: &str) -> Result<String> {
    let mut t = String::new();
    match target {
        Target::Issue(i) => {
            t.push_str(&format!("# {}: {}\n\n{}\n\n", i.key, i.title, i.body));
            let comments: Vec<String> = engine
                .db
                .issue_events(i.id)?
                .into_iter()
                .filter(|e| e["kind"] == "comment")
                .map(|e| format!("- {}: {}", e["actor"].as_str().unwrap_or("?"), e["body"].as_str().unwrap_or("")))
                .collect();
            if !comments.is_empty() {
                t.push_str(&format!("## Comments\n\n{}\n\n", comments.join("\n")));
            }
        }
        Target::Review(r) => {
            t.push_str(&format!("# Review !{}: {}\n\n{}\n\nBranch `{}` into `{}`.\n\n", r.number, r.title, r.body, r.source_branch, r.target_branch));
        }
    }
    match kind {
        Kind::Task => t.push_str(&format!(
            "## Your job\n\nMake the change in this checkout (branch `{branch}`). Commit your work or leave it uncommitted; moloko commits anything left over, pushes the branch and opens a review. Don't push yourself.\n"
        )),
        Kind::Fix => t.push_str("## Your job\n\nThe checks on this branch failed. The failing log is below. Fix the cause.\n"),
        Kind::Review => t.push_str(
            "## Your job\n\nReview the change. The diff is in $MOLOKO_DIFF_FILE. Write your verdict to $MOLOKO_REVIEW_FILE: the first line is APPROVE or REQUEST_CHANGES, then your comments.\n",
        ),
    }
    if !instruction.is_empty() {
        t.push_str(&format!("\n## Request\n\n{instruction}\n"));
    }
    Ok(t)
}

async fn prepare_workspace(engine: &Arc<Engine>, session: i64, target: &Target, kind: Kind) -> Result<(Workspace, String, String)> {
    let (project_id, primary_hint, branch) = match target {
        Target::Issue(i) => (i.project_id, None, crate::issues::branch_name(i)),
        Target::Review(r) => (r.project_id, Some(r.repo_id), r.source_branch.clone()),
    };
    let repos = engine.db.repos(project_id)?;
    if repos.is_empty() {
        bail!("the project has no repos");
    }
    // The repo that already has the branch, else the first repo with any commits.
    let mut primary = None;
    let mut first_with_commits = None;
    for r in &repos {
        let path = engine.git.ensure(r.id, &r.default_branch).await?;
        if Some(r.id) == primary_hint || (primary_hint.is_none() && engine.git.resolve(&path, &format!("refs/heads/{branch}")).await.is_ok()) {
            primary = Some(r.clone());
            break;
        }
        if first_with_commits.is_none() && engine.git.resolve(&path, &r.default_branch).await.is_ok() {
            first_with_commits = Some(r.clone());
        }
    }
    let primary = primary.or(first_with_commits).unwrap_or_else(|| repos[0].clone());
    let dir = engine.work_dir.join(format!("ai-{session}"));
    tokio::fs::create_dir_all(&dir).await?;
    let mut checkouts = Vec::new();
    let mut base_sha = String::new();
    for r in std::iter::once(primary.clone()).chain(repos.into_iter().filter(|r| r.id != primary.id)) {
        let path = engine.git.path(r.id);
        let sha = match engine.git.resolve(&path, &format!("refs/heads/{branch}")).await {
            Ok(s) => s,
            Err(_) => match engine.git.resolve(&path, &r.default_branch).await {
                Ok(s) => s,
                Err(_) if r.id != primary.id => continue,
                Err(_) => bail!("{} has no commits yet", r.name),
            },
        };
        let dest = dir.join(&r.name);
        engine.git.checkout(r.id, &sha, &dest).await?;
        if r.id == primary.id {
            base_sha = sha.clone();
        }
        checkouts.push((r, dest, sha));
    }
    let _ = kind;
    Ok((Workspace { dir, primary, checkouts }, branch, base_sha))
}

/// Run `script` in the agent's environment with live logs. Returns the exit code.
async fn run_in_env(
    engine: &Engine,
    agent: &AiAgent,
    session: i64,
    ws: &Workspace,
    script: &str,
    env: &[(String, String)],
    log: &crate::logs::JobLog,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<i32> {
    let timeout = std::time::Duration::from_secs(agent.timeout_secs.max(60) as u64);
    let workdir = ws.dir.join(&ws.primary.name);
    let name = format!("moloko-ai-{session}");
    let result = match agent.environment.kind.as_str() {
        "container" => {
            let exec = Executor::docker(&agent.environment.image, &name, &ws.dir, &format!("/workspace/{}", ws.primary.name), env, log).await?;
            let r = tokio::time::timeout(timeout, exec.run_step(script, &workdir, env, &|l| log.line(l), cancel)).await;
            exec.shutdown().await;
            r
        }
        "vm" => {
            let vm = Vm::new(agent, &name, &ws.dir)?;
            vm.start(log).await?;
            let inner = env.iter().map(|(k, v)| format!("export {k}={}", shell_quote(v))).collect::<Vec<_>>().join("\n");
            let inner = format!("{inner}\ncd {}/{}\n{script}", shell_quote(&vm.mount_point()), shell_quote(&ws.primary.name));
            let r = tokio::time::timeout(timeout, Executor::Host.run_step(&vm.exec_command(&inner), &ws.dir, &[], &|l| log.line(l), cancel)).await;
            vm.destroy().await;
            r
        }
        "host" => {
            log.line("▶ running on the host, without isolation");
            tokio::time::timeout(timeout, Executor::Host.run_step(script, &workdir, env, &|l| log.line(l), cancel)).await
        }
        other => bail!("unknown environment kind {other:?}: use container, vm or host"),
    };
    let _ = engine;
    match result {
        Err(_) => {
            log.line(&format!("✗ timed out after {}s", timeout.as_secs()));
            Ok(124)
        }
        Ok(Ok(StepResult::Exited(c))) => Ok(c),
        Ok(Ok(StepResult::Cancelled)) => Ok(130),
        Ok(Err(e)) => Err(e),
    }
}

/// A VM per session. `tart` is built in for macOS; anything else is a driver executable called as
/// `driver create <name> <image> <cpus> <memory> <workspace>`, `driver exec <name> <script>`,
/// `driver destroy <name>`, which mounts `<workspace>` at `/workspace` in the guest.
struct Vm {
    driver: String,
    name: String,
    image: String,
    cpus: u32,
    memory_mb: u64,
    workspace: PathBuf,
}

impl Vm {
    fn new(agent: &AiAgent, name: &str, workspace: &std::path::Path) -> Result<Vm> {
        let env = &agent.environment;
        let driver = env.driver.clone().unwrap_or_else(|| if cfg!(target_os = "macos") { "tart".into() } else { String::new() });
        if driver.is_empty() {
            bail!("vm environments need a driver on this platform (set environment.driver)");
        }
        let memory_mb = env
            .memory
            .as_deref()
            .map(|m| {
                let m = m.trim().to_uppercase();
                match m.strip_suffix('G') {
                    Some(g) => g.parse::<u64>().unwrap_or(4) * 1024,
                    None => m.trim_end_matches('M').parse::<u64>().unwrap_or(4096),
                }
            })
            .unwrap_or(4096);
        Ok(Vm { driver, name: name.into(), image: env.image.clone(), cpus: env.cpus.unwrap_or(2), memory_mb, workspace: workspace.to_path_buf() })
    }

    fn mount_point(&self) -> String {
        if self.driver == "tart" { "/Volumes/My Shared Files/workspace".into() } else { "/workspace".into() }
    }

    async fn sh(&self, script: &str) -> Result<()> {
        let out = tokio::process::Command::new("sh").args(["-c", script]).output().await?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    async fn start(&self, log: &crate::logs::JobLog) -> Result<()> {
        let (n, img, ws) = (shell_quote(&self.name), shell_quote(&self.image), shell_quote(&self.workspace.display().to_string()));
        log.line(&format!("▶ vm: starting {} with {}", self.image, self.driver));
        if self.driver == "tart" {
            self.sh(&format!("tart clone {img} {n} && tart set {n} --cpu {} --memory {}", self.cpus, self.memory_mb)).await.context("tart clone")?;
            // `tart run` stays in the foreground for the VM's lifetime.
            tokio::process::Command::new("tart")
                .args(["run", "--no-graphics", &format!("--dir=workspace:{}", self.workspace.display()), &self.name])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("tart run")?;
            for _ in 0..120 {
                if self.sh(&format!("tart exec {n} true")).await.is_ok() {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            bail!("the VM didn't come up within 2 minutes");
        }
        let d = shell_quote(&self.driver);
        self.sh(&format!("{d} create {n} {img} {} {} {ws}", self.cpus, self.memory_mb)).await.context("vm driver create")
    }

    fn exec_command(&self, inner: &str) -> String {
        if self.driver == "tart" {
            format!("tart exec {} sh -e -c {}", shell_quote(&self.name), shell_quote(inner))
        } else {
            format!("{} exec {} {}", shell_quote(&self.driver), shell_quote(&self.name), shell_quote(inner))
        }
    }

    async fn destroy(&self) {
        let n = shell_quote(&self.name);
        let script = if self.driver == "tart" {
            format!("tart stop {n}; tart delete {n}")
        } else {
            format!("{} destroy {n}", shell_quote(&self.driver))
        };
        let _ = self.sh(&script).await;
    }
}

async fn run_session(engine: &Arc<Engine>, agent: &AiAgent, session: i64, target: Target, kind: Kind, instruction: &str) -> Result<()> {
    let bot = engine.db.user(agent.user_id)?.ok_or_else(|| anyhow!("agent user is gone"))?;
    let (ws, branch, base_sha) = prepare_workspace(engine, session, &target, kind).await?;
    let run_id = engine.db.create_run(agent.project_id, ws.primary.id, &format!("refs/heads/{branch}"), &format!("ai:{}", agent.name))?;
    engine.db.start_run(run_id, &base_sha)?;
    let job_id = engine.db.create_job(run_id, &format!("{}-{}", agent.name, kind.as_str()), &[])?;
    engine.db.conn().execute("UPDATE ai_sessions SET run_id = ?2, branch = ?3, state = 'running' WHERE id = ?1", params![session, run_id, branch])?;
    let cancel = engine.register_cancel(run_id);
    let log = engine.logs.open(job_id)?;
    engine.db.start_job(job_id)?;
    log.line(&format!("▶ @{} {} session #{session} on {} ({})", agent.name, kind.as_str(), branch, ws.primary.name));

    let outcome = session_body(engine, agent, &bot, session, &target, kind, instruction, &ws, &branch, &base_sha, &log, &cancel).await;
    let (status, summary) = match &outcome {
        Ok(s) => (if s.exit == 0 { Status::Success } else { Status::Failed }, s.summary.clone()),
        Err(e) => {
            log.line(&format!("✗ {e:#}"));
            (Status::Failed, format!("{e:#}"))
        }
    };
    let status = if cancel.is_cancelled() { Status::Cancelled } else { status };
    log.line(&format!("● {}", status.as_str()));
    drop(log);
    engine.logs.close(job_id);
    engine.db.finish_job(job_id, status, outcome.as_ref().ok().map(|s| s.exit))?;
    engine.db.finish_run(run_id, status, None)?;
    engine.unregister_cancel(run_id);
    for (r, dest, _) in &ws.checkouts {
        engine.git.remove(r.id, dest).await;
    }
    let _ = tokio::fs::remove_dir_all(&ws.dir).await;
    engine.db.set_session(session, status.as_str(), Some(&summary))?;

    let note = format!("{summary}\n\nLog: run #{run_id}");
    match &target {
        Target::Issue(i) => {
            engine.db.add_issue_event(i.id, Some(bot.id), "comment", &note, Some(json!({ "session": session })))?;
        }
        Target::Review(r) => {
            if kind != Kind::Review {
                engine.db.add_review_comment(r.id, bot.id, &note, None, None)?;
            }
        }
    }
    Ok(())
}

struct SessionOutcome {
    exit: i32,
    summary: String,
}

#[allow(clippy::too_many_arguments)]
async fn session_body(
    engine: &Arc<Engine>,
    agent: &AiAgent,
    bot: &User,
    session: i64,
    target: &Target,
    kind: Kind,
    instruction: &str,
    ws: &Workspace,
    branch: &str,
    base_sha: &str,
    log: &crate::logs::JobLog,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<SessionOutcome> {
    let mut extra = instruction.to_string();
    if kind == Kind::Fix
        && let Some(run) = engine.db.run_for_sha(ws.primary.id, base_sha)?
    {
        for j in engine.db.jobs(run.id)?.into_iter().filter(|j| j.status == "failed") {
            if let crate::logs::Subscription::Done(text) = engine.logs.subscribe(j.id) {
                let tail: Vec<&str> = text.lines().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect();
                extra.push_str(&format!("\n\n### Failed job {}\n\n```\n{}\n```\n", j.name, tail.join("\n")));
            }
        }
    }
    let task = task_text(engine, target, kind, &extra, branch)?;
    let task_file = ws.dir.join(".moloko-task.md");
    tokio::fs::write(&task_file, &task).await?;
    let (diff_file, review_file) = (ws.dir.join(".moloko-diff.patch"), ws.dir.join(".moloko-review.md"));
    if let Target::Review(r) = target {
        let path = engine.git.path(r.repo_id);
        let mb = crate::git::git(&["merge-base", &format!("refs/heads/{}", r.target_branch), &r.head_sha], Some(&path)).await.unwrap_or_default();
        let (_, diff) = crate::code::diff(&path, &mb, &r.head_sha).await.unwrap_or_default();
        tokio::fs::write(&diff_file, diff).await?;
    }

    // A token that may only push this branch, valid for the session.
    let prefix = branch.to_string();
    let scope = Scope { project_id: Some(agent.project_id), push_prefix: Some(prefix) };
    let (_, token) = engine.db.create_token(bot.id, &format!("session {session}"), Some(&scope), Some(agent.timeout_secs * 1000 + 600_000))?;
    let registry_token = crate::agents::new_token();
    engine.grant_registry(&registry_token);
    let in_env = |p: &std::path::Path| -> String {
        let rel = p.strip_prefix(&ws.dir).unwrap_or(p).display().to_string();
        match agent.environment.kind.as_str() {
            "vm" if agent.environment.driver.as_deref().unwrap_or("tart") == "tart" && cfg!(target_os = "macos") => format!("/Volumes/My Shared Files/workspace/{rel}"),
            "host" => p.display().to_string(),
            _ => format!("/workspace/{rel}"),
        }
    };
    let mut env: Vec<(String, String)> = vec![
        ("CI".into(), "true".into()),
        ("MOLOKO".into(), "true".into()),
        ("MOLOKO_AGENT".into(), agent.name.clone()),
        ("MOLOKO_SESSION".into(), session.to_string()),
        ("MOLOKO_BRANCH".into(), branch.into()),
        ("MOLOKO_TASK_FILE".into(), in_env(&task_file)),
        ("MOLOKO_DIFF_FILE".into(), in_env(&diff_file)),
        ("MOLOKO_REVIEW_FILE".into(), in_env(&review_file)),
        ("MOLOKO_TOKEN".into(), token.clone()),
        ("MOLOKO_REGISTRY".into(), engine.registry_host.clone()),
        ("MOLOKO_REGISTRY_USER".into(), "moloko".into()),
        ("MOLOKO_REGISTRY_TOKEN".into(), registry_token.clone()),
        ("GIT_AUTHOR_NAME".into(), bot.display_name.clone()),
        ("GIT_AUTHOR_EMAIL".into(), format!("{}@moloko", bot.username)),
        ("GIT_COMMITTER_NAME".into(), bot.display_name.clone()),
        ("GIT_COMMITTER_EMAIL".into(), format!("{}@moloko", bot.username)),
    ];
    for name in &agent.secrets {
        match engine.db.secret(agent.project_id, name)? {
            Some(sealed) => env.push((name.clone(), engine.secrets.open(&sealed)?)),
            None => log.line(&format!("✗ secret {name} is not set for this project")),
        }
    }
    let command = match kind {
        Kind::Review => agent.review_command.clone().unwrap_or_else(|| agent.command.clone()),
        _ => agent.command.clone(),
    };
    let exit = run_in_env(engine, agent, session, ws, &command, &env, log, cancel).await;
    engine.revoke_registry(&registry_token);
    let _ = engine.db.delete_token_by_value(&token);
    let exit = exit?;
    log.line(&format!("▶ agent exited with {exit}"));

    if kind == Kind::Review {
        let Target::Review(r) = target else { unreachable!() };
        let verdict = tokio::fs::read_to_string(&review_file).await.unwrap_or_default();
        let mut lines = verdict.lines();
        let first = lines.next().unwrap_or("").trim().to_uppercase();
        let rest: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();
        let state = if first.starts_with("APPROVE") { "approved" } else if first.starts_with("REQUEST") { "changes_requested" } else { "" };
        if !rest.is_empty() {
            engine.db.add_review_comment(r.id, bot.id, &rest, None, None)?;
        }
        if !state.is_empty() {
            engine.db.set_approval(r.id, bot.id, state, &r.head_sha)?;
        }
        let summary = if state.is_empty() { "The AI review gave no verdict.".to_string() } else { format!("AI review: {state}.") };
        return Ok(SessionOutcome { exit, summary });
    }

    // Commit what's left, then push every repo that changed and open or update its review.
    let mut pushed = Vec::new();
    let actor = Actor { user: bot.clone(), scope: Some(Scope { project_id: Some(agent.project_id), push_prefix: Some(branch.to_string()) }) };
    let ident = crate::git::identity_env(&bot.display_name, &format!("{}@moloko", bot.username));
    for (repo, dest, start_sha) in &ws.checkouts {
        let status = crate::git::git(&["status", "--porcelain"], Some(dest)).await.unwrap_or_default();
        if !status.trim().is_empty() {
            let title = match target {
                Target::Issue(i) => format!("{}: {}", i.key, i.title),
                Target::Review(r) => format!("Address review on !{}", r.number),
            };
            crate::git::git_env(&["add", "-A"], Some(dest), &ident).await?;
            crate::git::git_env(&["commit", "-q", "-m", &format!("{title}\n\nAI agent session #{session}")], Some(dest), &ident).await?;
        }
        let head = crate::git::git(&["rev-parse", "HEAD"], Some(dest)).await?;
        if &head == start_sha {
            continue;
        }
        let path = engine.git.path(repo.id);
        let existing = engine.git.resolve(&path, &format!("refs/heads/{branch}")).await.ok();
        let old = match &existing {
            Some(e) if e == start_sha => e.clone(),
            Some(_) => {
                log.line(&format!("✗ {branch} moved in {} during the session; not pushing", repo.name));
                continue;
            }
            None => crate::repos::zero(),
        };
        let res = engine
            .apply_ref_updates(repo, Some(&actor), &[RefUpdate { name: format!("refs/heads/{branch}"), old, new: head.clone() }], Source::Push, true)
            .await?;
        if let Some(Err(e)) = res.into_iter().next() {
            log.line(&format!("✗ push to {} rejected: {e}", repo.name));
            continue;
        }
        let n = crate::git::git(&["rev-list", "--count", &format!("{start_sha}..{head}")], Some(&path)).await.unwrap_or_default();
        log.line(&format!("▶ pushed {n} commit(s) to {} {branch}", repo.name));
        let review = crate::reviews::open(engine, bot, &crate::reviews::NewReview {
            repo_id: repo.id,
            source_branch: branch.to_string(),
            target_branch: None,
            title: None,
            body: Some(format!("Opened by @{} (session #{session}).", agent.name)),
            draft: false,
        })
        .await;
        match review {
            Ok(r) => pushed.push(format!("{} (review !{})", repo.name, r.number)),
            Err(e) => {
                log.line(&format!("✗ couldn't open a review: {e:#}"));
                pushed.push(repo.name.clone());
            }
        }
    }
    let summary = if pushed.is_empty() {
        if exit == 0 { "Finished without changes.".to_string() } else { format!("The agent exited with {exit} and made no changes.") }
    } else {
        format!("Pushed `{branch}` to {}.", pushed.join(", "))
    };
    Ok(SessionOutcome { exit, summary })
}

// ---------- triggers ----------

pub async fn on_assigned(engine: &Arc<Engine>, issue: &Issue) -> Result<()> {
    let Some(uid) = issue.assignee_id else { return Ok(()) };
    if let Some(agent) = engine.db.ai_agent_for_user(issue.project_id, uid)? {
        start(engine, agent, Target::Issue(issue.clone()), Kind::Task, String::new())?;
    }
    Ok(())
}

fn mentioned(body: &str, name: &str) -> bool {
    let needle = format!("@{}", name.to_lowercase());
    let lower = body.to_lowercase();
    lower.match_indices(&needle).any(|(i, _)| {
        let after = lower[i + needle.len()..].chars().next();
        !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// `@coder please …` on an issue, or on a review that belongs to one.
pub async fn on_mention(engine: &Arc<Engine>, issue: &Issue, review: Option<&Review>, author: &User, body: &str) -> Result<()> {
    for agent in engine.db.ai_agents(issue.project_id)? {
        if agent.user_id == author.id || !mentioned(body, &agent.name) {
            continue;
        }
        // On the agent's own review, follow up on that branch; otherwise work on the issue.
        let target = match review {
            Some(r) if r.author_id == Some(agent.user_id) => Target::Review(r.clone()),
            Some(r) => Target::Review(r.clone()),
            None => Target::Issue(issue.clone()),
        };
        start(engine, agent, target, Kind::Task, body.to_string())?;
    }
    Ok(())
}

pub async fn on_review_mention(engine: &Arc<Engine>, review: &Review, author: &User, body: &str) -> Result<()> {
    for agent in engine.db.ai_agents(review.project_id)? {
        if agent.user_id != author.id && mentioned(body, &agent.name) {
            start(engine, agent, Target::Review(review.clone()), Kind::Task, body.to_string())?;
        }
    }
    Ok(())
}

/// New review or new commits: ask the configured AI reviewer for a review.
pub async fn on_review_updated(engine: &Arc<Engine>, review: &Review) -> Result<()> {
    if review.state != "open" {
        return Ok(());
    }
    let path = engine.git.path(review.repo_id);
    let rules = match engine.git.read_file(&path, &format!("refs/heads/{}", review.target_branch), crate::config::PIPELINE_FILE).await {
        Ok(src) => crate::config::Pipeline::parse(&src).map(|p| p.review).unwrap_or_default(),
        Err(_) => return Ok(()),
    };
    let (Some(name), false) = (rules.ai_reviewer.as_deref(), rules.ai_review == "off") else { return Ok(()) };
    let Some(agent) = engine.db.ai_agents(review.project_id)?.into_iter().find(|a| a.name == name) else { return Ok(()) };
    if review.author_id == Some(agent.user_id) {
        return Ok(());
    }
    start(engine, agent, Target::Review(review.clone()), Kind::Review, String::new())?;
    Ok(())
}

pub async fn on_changes_requested(engine: &Arc<Engine>, review: &Review, by: &User, text: &str) -> Result<()> {
    let Some(author) = review.author_id else { return Ok(()) };
    if let Some(agent) = engine.db.ai_agent_for_user(review.project_id, author)?
        && agent.user_id != by.id
    {
        start(engine, agent, Target::Review(review.clone()), Kind::Task, format!("{} requested changes: {text}", by.username))?;
    }
    Ok(())
}

/// Failing checks on an agent's own review: let it try to fix them (at most 3 times in a row).
pub async fn on_run_finished(engine: &Arc<Engine>, ctx: &Arc<RunCtx>, status: Status) -> Result<()> {
    if status != Status::Failed {
        return Ok(());
    }
    let Some(branch) = ctx.git_ref.strip_prefix("refs/heads/") else { return Ok(()) };
    for r in engine.db.active_reviews_for_branch(ctx.repo.id, branch)? {
        let Some(author) = r.author_id else { continue };
        let Some(agent) = engine.db.ai_agent_for_user(r.project_id, author)? else { continue };
        if !agent.auto_fix_ci || r.head_sha != ctx.sha {
            continue;
        }
        let fixes: i64 = engine.db.conn().query_row(
            "SELECT COUNT(*) FROM ai_sessions WHERE review_id = ?1 AND kind = 'fix' AND created_at > ?2",
            params![r.id, r.created_at],
            |x| x.get(0),
        )?;
        if fixes < 3 {
            start(engine, agent, Target::Review(r.clone()), Kind::Fix, String::new())?;
        }
    }
    Ok(())
}

// ---------- API ----------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects/{id}/ai-agents", get(list_agents).post(create_agent))
        .route("/api/ai-agents/{id}", patch(update_agent).delete(delete_agent))
        .route("/api/projects/{id}/secrets", get(list_secrets))
        .route("/api/projects/{id}/secrets/{name}", put(put_secret).delete(delete_secret))
        .route("/api/projects/{id}/ai-sessions", get(list_sessions))
        .route("/api/issues/{id}/ai", post(start_on_issue))
}

#[derive(Deserialize)]
struct AgentBody {
    name: Option<String>,
    environment: Option<Environment>,
    command: Option<String>,
    review_command: Option<String>,
    secrets: Option<Vec<String>>,
    timeout_secs: Option<i64>,
    max_sessions: Option<i64>,
    auto_fix_ci: Option<bool>,
}

fn check_env(e: &Environment) -> ApiResult<()> {
    if !["container", "vm", "host"].contains(&e.kind.as_str()) {
        return Err(bad_request("environment.kind must be container, vm or host"));
    }
    if e.kind != "host" && e.image.trim().is_empty() {
        return Err(bad_request("environment.image is required"));
    }
    Ok(())
}

async fn list_agents(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    Ok(Json(s.engine.db.ai_agents(id)?))
}

async fn create_agent(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(b): Json<AgentBody>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad_request("name is required"))?;
    let env = b.environment.ok_or_else(|| bad_request("environment is required"))?;
    check_env(&env)?;
    let command = b.command.filter(|c| !c.trim().is_empty()).ok_or_else(|| bad_request("command is required"))?;
    let uid = s.engine.db.create_user(name, None, "bot", false).map_err(|e| bad_request(format!("{e:#}")))?;
    s.engine.db.set_member(id, uid, Role::Write)?;
    let aid = {
        let c = s.engine.db.conn();
        c.execute(
            "INSERT INTO ai_agents (project_id, user_id, environment, command, review_command, secrets, timeout_secs, max_sessions, auto_fix_ci, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![id, uid, serde_json::to_string(&env).unwrap(), command, b.review_command, serde_json::to_string(&b.secrets.unwrap_or_default()).unwrap(),
                b.timeout_secs.unwrap_or(2700).clamp(60, 86400), b.max_sessions.unwrap_or(1).clamp(1, 32), b.auto_fix_ci.unwrap_or(false) as i64, now_ms()],
        )?;
        c.last_insert_rowid()
    };
    Ok((StatusCode::CREATED, Json(s.engine.db.ai_agent(aid)?)))
}

async fn agent_for(s: &AppState, actor: &Actor, id: i64) -> ApiResult<AiAgent> {
    let a = s.engine.db.ai_agent(id)?.ok_or_else(not_found)?;
    need(s, actor, a.project_id, Role::Admin)?;
    Ok(a)
}

async fn update_agent(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(b): Json<AgentBody>) -> ApiResult<impl IntoResponse> {
    agent_for(&s, &actor, id).await?;
    let c = s.engine.db.conn();
    if let Some(e) = &b.environment {
        check_env(e)?;
        c.execute("UPDATE ai_agents SET environment = ?2 WHERE id = ?1", params![id, serde_json::to_string(e).unwrap()])?;
    }
    if let Some(v) = b.command.filter(|c| !c.trim().is_empty()) { c.execute("UPDATE ai_agents SET command = ?2 WHERE id = ?1", params![id, v])?; }
    if let Some(v) = b.review_command { c.execute("UPDATE ai_agents SET review_command = ?2 WHERE id = ?1", params![id, Some(v).filter(|v| !v.is_empty())])?; }
    if let Some(v) = b.secrets { c.execute("UPDATE ai_agents SET secrets = ?2 WHERE id = ?1", params![id, serde_json::to_string(&v).unwrap()])?; }
    if let Some(v) = b.timeout_secs { c.execute("UPDATE ai_agents SET timeout_secs = ?2 WHERE id = ?1", params![id, v.clamp(60, 86400)])?; }
    if let Some(v) = b.max_sessions { c.execute("UPDATE ai_agents SET max_sessions = ?2 WHERE id = ?1", params![id, v.clamp(1, 32)])?; }
    if let Some(v) = b.auto_fix_ci { c.execute("UPDATE ai_agents SET auto_fix_ci = ?2 WHERE id = ?1", params![id, v as i64])?; }
    drop(c);
    s.engine.ai.slots.lock().unwrap().remove(&id);
    Ok(Json(s.engine.db.ai_agent(id)?))
}

async fn delete_agent(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    let a = agent_for(&s, &actor, id).await?;
    let c = s.engine.db.conn();
    c.execute("DELETE FROM ai_agents WHERE id = ?1", [id])?;
    c.execute("DELETE FROM members WHERE user_id = ?1 AND project_id = ?2", params![a.user_id, a.project_id])?;
    c.execute("DELETE FROM tokens WHERE user_id = ?1", [a.user_id])?;
    Ok(StatusCode::NO_CONTENT)
}

fn valid_secret_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && !n.starts_with(|c: char| c.is_ascii_digit())
}

async fn list_secrets(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    Ok(Json(s.engine.db.secret_names(id)?))
}

#[derive(Deserialize)]
struct SecretBody {
    value: String,
}

async fn put_secret(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path((id, name)): Path<(i64, String)>, Json(b): Json<SecretBody>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    if !valid_secret_name(&name) {
        return Err(bad_request("secret names are UPPER_SNAKE_CASE"));
    }
    s.engine.db.set_secret(id, &name, &s.engine.secrets.seal(&b.value))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_secret(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path((id, name)): Path<(i64, String)>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    s.engine.db.conn().execute("DELETE FROM project_secrets WHERE project_id = ?1 AND name = ?2", params![id, name])?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    Ok(Json(s.engine.db.sessions_where("a.project_id = ?1", [id])?))
}

#[derive(Deserialize)]
struct StartBody {
    agent: String,
    #[serde(default)]
    instruction: String,
}

async fn start_on_issue(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(b): Json<StartBody>) -> ApiResult<impl IntoResponse> {
    let issue = s.engine.db.issue(id)?.ok_or_else(not_found)?;
    need(&s, &actor, issue.project_id, Role::Write)?;
    let agent = s.engine.db.ai_agents(issue.project_id)?.into_iter().find(|a| a.name == b.agent).ok_or_else(|| bad_request("no such AI agent"))?;
    let sid = start(&s.engine, agent, Target::Issue(issue), Kind::Task, b.instruction)?;
    Ok((StatusCode::CREATED, Json(json!({ "session": sid }))))
}

#[cfg(test)]
mod tests {
    #[test]
    fn mentions() {
        assert!(super::mentioned("hey @coder please fix", "coder"));
        assert!(super::mentioned("@Coder.", "coder"));
        assert!(!super::mentioned("@coders", "coder"));
        assert!(!super::mentioned("coder", "coder"));
    }
}
