//! One job instance (one matrix combination): start the runner container and services,
//! run the steps, then the post steps, and clean up.
//!
//! Every host path the job uses (workspace, runner temp, git mirror, fetched actions) is
//! bind-mounted at the *same* path inside the container, so paths never need translating,
//! and `docker run -v $PWD:...` from inside a step (via the mounted Docker socket) works.

use super::commands::{self, Command as WfCommand};
use super::expr::{self, Ctx, StatusCtx};
use super::workflow::{Action, Job, Step};
use super::{RunSpec, short_ref};
use crate::db::Status;
use crate::engine::Engine;
use crate::executor::{StepResult, stream};
use crate::logs::JobLog;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DOCKER_SOCK: &str = "/var/run/docker.sock";
const MAX_COMPOSITE_DEPTH: usize = 10;

pub struct Instance {
    pub engine: Arc<Engine>,
    pub spec: Arc<RunSpec>,
    pub job_id: String,
    pub job: Job,
    pub row: i64,
    pub matrix: Map<String, Value>,
    pub strategy: Value,
    pub needs: Value,
    pub cancel: CancellationToken,
}

/// Run the instance; returns its status and the job's `outputs`.
pub async fn run(inst: Instance) -> (Status, Map<String, Value>) {
    let _permit = tokio::select! {
        p = inst.engine.slots.clone().acquire_owned() => p.expect("semaphore closed"),
        _ = inst.cancel.cancelled() => {
            let _ = inst.engine.db.finish_job(inst.row, Status::Cancelled, None);
            return (Status::Cancelled, Map::new());
        }
    };
    let log = match inst.engine.logs.open(inst.row) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(job = inst.row, "cannot open log: {e}");
            let _ = inst.engine.db.finish_job(inst.row, Status::Failed, None);
            return (Status::Failed, Map::new());
        }
    };
    let _ = inst.engine.db.start_job(inst.row);
    let started = Instant::now();
    let mut jr = JobRun::new(&inst, log.clone());
    let (status, outputs) = jr.execute().await;
    jr.cleanup().await;
    log.line(&format!(
        "● {} in {:.1}s",
        status.as_str(),
        started.elapsed().as_secs_f64()
    ));
    drop(jr);
    drop(log);
    inst.engine.logs.close(inst.row);
    let _ = inst.engine.db.finish_job(inst.row, status, None);
    (status, outputs)
}

/// Per-scope state: the job itself, or a composite action's steps.
struct Scope {
    steps: Map<String, Value>,
    inputs: Map<String, Value>,
    env: Vec<(String, String)>,
    action_path: Option<PathBuf>,
    failed: bool,
    depth: usize,
    composite: bool,
}

struct RunnerSpec {
    /// Image name, or `host`.
    image: String,
    env: Map<String, Value>,
    opts: String,
    vols: Vec<String>,
    note: Option<String>,
}

struct PostStep {
    name: String,
    cond: String,
    kind: PostKind,
    env: Vec<(String, String)>,
}

enum PostKind {
    Process(Vec<String>),
    Container {
        image: String,
        entrypoint: Option<String>,
        args: Vec<String>,
    },
    CacheSave {
        key: String,
        paths: Vec<String>,
    },
}

#[derive(Default)]
struct Capture {
    outputs: Map<String, Value>,
    state: Vec<(String, String)>,
}

struct Files {
    env: PathBuf,
    output: PathBuf,
    path: PathBuf,
    state: PathBuf,
    summary: PathBuf,
}

type Outcome = (&'static str, Map<String, Value>);

struct JobRun<'a> {
    inst: &'a Instance,
    log: Arc<JobLog>,
    ws: PathBuf,
    temp: PathBuf,
    mirror: PathBuf,
    container: String,
    container_started: bool,
    /// Run steps directly on the build machine instead of in a container (`runs-on` mapped to `host`).
    host: bool,
    /// On macOS, container jobs keep their workspace in this Docker volume (native Linux disk
    /// in Docker's VM) instead of a macOS folder shared over virtiofs, which is
    /// case-insensitive and drops files under heavy parallel writes (npm ci).
    volume: Option<String>,
    /// hashFiles() for a workspace that only exists inside the container.
    hasher: Option<Box<expr::Hasher>>,
    services: Vec<(String, String)>,
    base_path: String,
    home: String,
    base_env: Vec<(String, String)>,
    file_env: Vec<(String, String)>,
    path_add: Vec<String>,
    post: Vec<PostStep>,
    worktrees: Vec<PathBuf>,
    github: Value,
    runner: Value,
    secrets: Value,
    job_env: Vec<(String, String)>,
    masks: Mutex<Vec<String>>,
    capture: Mutex<Capture>,
    stop_token: Mutex<Option<String>>,
    seq: AtomicUsize,
}

fn set(list: &mut Vec<(String, String)>, k: &str, v: String) {
    match list.iter_mut().find(|(n, _)| n == k) {
        Some(e) => e.1 = v,
        None => list.push((k.to_string(), v)),
    }
}

fn merged(layers: &[&[(String, String)]]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for l in layers {
        for (k, v) in l.iter() {
            set(&mut out, k, v.clone());
        }
    }
    out
}

fn env_value(list: &[(String, String)]) -> Value {
    Value::Object(list.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
}

fn cond_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn lines(s: Option<&str>) -> Vec<String> {
    s.unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Split a command line into words, honouring single/double quotes and backslashes.
pub fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

enum Uses {
    Docker(String),
    Local(String),
    Remote {
        owner: String,
        repo: String,
        path: Option<String>,
        git_ref: String,
    },
}

fn parse_uses(s: &str) -> Result<Uses> {
    let s = s.trim();
    if let Some(img) = s.strip_prefix("docker://") {
        return Ok(Uses::Docker(img.to_string()));
    }
    if s.starts_with("./") || s.starts_with("../") {
        return Ok(Uses::Local(s.to_string()));
    }
    let (name, git_ref) = s
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("uses: {s:?} needs a version, e.g. {s}@v1"))?;
    let mut parts = name.splitn(3, '/');
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
        bail!("uses: {s:?} must look like owner/repo@ref");
    };
    if owner.is_empty() || repo.is_empty() || git_ref.is_empty() {
        bail!("uses: {s:?} must look like owner/repo@ref");
    }
    Ok(Uses::Remote {
        owner: owner.to_string(),
        repo: repo.to_string(),
        path: parts.next().map(str::to_string),
        git_ref: git_ref.to_string(),
    })
}

async fn docker_out(args: &[String]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .await
        .context("failed to run docker")?;
    if !out.status.success() {
        bail!(
            "docker {}: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

impl<'a> JobRun<'a> {
    fn new(inst: &'a Instance, log: Arc<JobLog>) -> Self {
        let work = &inst.engine.work_dir;
        let mirror = inst.engine.git.mirror_path(inst.spec.repo.id);
        JobRun {
            inst,
            log,
            ws: work.join(inst.row.to_string()),
            temp: work.join(format!("{}-temp", inst.row)),
            mirror: mirror.canonicalize().unwrap_or(mirror),
            container: format!("moloko-gha-{}", inst.row),
            container_started: false,
            host: false,
            volume: None,
            hasher: None,
            services: Vec::new(),
            base_path: String::new(),
            home: "/root".into(),
            base_env: Vec::new(),
            file_env: Vec::new(),
            path_add: Vec::new(),
            post: Vec::new(),
            worktrees: Vec::new(),
            github: Value::Null,
            runner: Value::Null,
            secrets: Value::Null,
            job_env: Vec::new(),
            masks: Mutex::default(),
            capture: Mutex::default(),
            stop_token: Mutex::default(),
            seq: AtomicUsize::new(0),
        }
    }

    fn next(&self) -> usize {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn line(&self, s: &str) {
        self.log.line(&self.masked(s));
    }

    fn masked(&self, s: &str) -> String {
        let masks = self.masks.lock().unwrap();
        let mut out = s.to_string();
        for m in masks.iter() {
            if !m.is_empty() {
                out = out.replace(m.as_str(), "***");
            }
        }
        out
    }

    /// Process one line of step output: workflow commands, masking, logging.
    fn handle_line(&self, line: &str) {
        {
            let mut stop = self.stop_token.lock().unwrap();
            if let Some(tok) = stop.as_ref() {
                if line.trim() == format!("::{tok}::") {
                    *stop = None;
                } else {
                    drop(stop);
                    self.line(line);
                }
                return;
            }
        }
        match commands::parse_command(line) {
            None => self.line(line),
            Some(cmd) => match cmd {
                WfCommand::SetOutput { name, value } => {
                    self.capture.lock().unwrap().outputs.insert(name, Value::String(value));
                }
                WfCommand::SaveState { name, value } => self.capture.lock().unwrap().state.push((name, value)),
                WfCommand::AddMask(v) => {
                    if !v.trim().is_empty() {
                        self.masks.lock().unwrap().push(v);
                    }
                }
                WfCommand::Group(t) => self.line(&format!("▸ {t}")),
                WfCommand::EndGroup | WfCommand::Debug(_) | WfCommand::Ignore => {}
                WfCommand::Error(m) => self.line(&format!("✗ {m}")),
                WfCommand::Warning(m) => self.line(&format!("⚠ {m}")),
                WfCommand::Notice(m) => self.line(&format!("ℹ {m}")),
                WfCommand::StopCommands(t) => *self.stop_token.lock().unwrap() = Some(t),
            },
        }
    }

    // ---------- contexts ----------

    fn contexts(&self, scope: &Scope, step_env: &[(String, String)]) -> Map<String, Value> {
        let mut github = self.github.clone();
        if let Some(p) = &scope.action_path {
            github["action_path"] = Value::String(p.display().to_string());
        }
        let services: Map<String, Value> = self
            .services
            .iter()
            .map(|(k, name)| (k.clone(), json!({ "id": name, "network": "", "ports": {} })))
            .collect();
        let mut m = Map::new();
        m.insert("github".into(), github);
        m.insert("env".into(), env_value(&merged(&[&scope.env, &self.file_env, step_env])));
        m.insert("vars".into(), json!({}));
        m.insert(
            "job".into(),
            json!({
                "status": if scope.failed { "failure" } else { "success" },
                "container": { "id": self.container, "network": "" },
                "services": services,
            }),
        );
        m.insert("steps".into(), Value::Object(scope.steps.clone()));
        m.insert("runner".into(), self.runner.clone());
        m.insert("secrets".into(), self.secrets.clone());
        m.insert("strategy".into(), self.inst.strategy.clone());
        m.insert("matrix".into(), Value::Object(self.inst.matrix.clone()));
        m.insert("needs".into(), self.inst.needs.clone());
        m.insert("inputs".into(), Value::Object(scope.inputs.clone()));
        m
    }

    fn ctx<'c>(&'c self, contexts: &'c Map<String, Value>, failed: bool) -> Ctx<'c> {
        Ctx {
            contexts,
            status: StatusCtx {
                failed,
                cancelled: self.inst.cancel.is_cancelled(),
            },
            workspace: Some(&self.ws),
            hasher: self.hasher.as_deref(),
        }
    }

    /// Contexts for job-level fields (env, runs-on, container, services) before any step ran.
    fn job_contexts(&self, env: &[(String, String)]) -> Map<String, Value> {
        let scope = Scope {
            steps: Map::new(),
            inputs: self.inst.spec.inputs.clone(),
            env: env.to_vec(),
            action_path: None,
            failed: false,
            depth: 0,
            composite: false,
        };
        self.contexts(&scope, &[])
    }

    // ---------- job ----------

    async fn execute(&mut self) -> (Status, Map<String, Value>) {
        if let Err(e) = self.setup().await {
            self.line(&format!("✗ {e:#}"));
            return (
                if self.inst.cancel.is_cancelled() { Status::Cancelled } else { Status::Failed },
                Map::new(),
            );
        }
        let timeout = {
            let c = self.job_contexts(&self.job_env);
            self.inst
                .job
                .timeout_minutes
                .as_ref()
                .and_then(|v| expr::interpolate_value(v, &self.ctx(&c, false)).ok())
                .and_then(|v| v.as_f64().or_else(|| expr::to_string(&v).parse().ok()))
                .unwrap_or(360.0)
        };
        let timed_out = Arc::new(AtomicBool::new(false));
        let timer = {
            let (flag, cancel) = (timed_out.clone(), self.inst.cancel.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(timeout * 60.0)).await;
                flag.store(true, Ordering::SeqCst);
                cancel.cancel();
            })
        };

        let mut scope = Scope {
            steps: Map::new(),
            inputs: self.inst.spec.inputs.clone(),
            env: self.job_env.clone(),
            action_path: None,
            failed: false,
            depth: 0,
            composite: false,
        };
        let steps = self.inst.job.steps.clone();
        self.run_steps(&steps, &mut scope).await;
        self.run_post(&mut scope).await;
        timer.abort();

        let contexts = self.contexts(&scope, &[]);
        let ctx = self.ctx(&contexts, scope.failed);
        let mut outputs = Map::new();
        for (k, v) in &self.inst.job.outputs {
            match expr::interpolate_value(v, &ctx) {
                Ok(v) => {
                    outputs.insert(k.clone(), Value::String(expr::to_string(&v)));
                }
                Err(e) => self.line(&format!("⚠ output {k}: {e:#}")),
            }
        }
        let status = if timed_out.load(Ordering::SeqCst) {
            self.line(&format!("✗ job timed out after {timeout} minutes"));
            Status::Failed
        } else if self.inst.cancel.is_cancelled() {
            Status::Cancelled
        } else if scope.failed {
            Status::Failed
        } else {
            Status::Success
        };
        (status, outputs)
    }

    async fn setup(&mut self) -> Result<()> {
        let spec = self.inst.spec.clone();
        let spec = &spec;
        let engine = self.inst.engine.clone();
        let engine = &engine;
        let runner = self.resolve_runner()?;
        if let Some(n) = &runner.note {
            self.line(&format!("⚠ {n}"));
        }
        if runner.image != "host" && cfg!(target_os = "macos") {
            let v = format!("moloko-ws-{}", self.inst.row);
            let _ = docker_out(&strs(&["volume", "rm", "-f", &v])).await;
            docker_out(&strs(&["volume", "create", &v])).await.context("creating the workspace volume")?;
            self.volume = Some(v);
        }
        for d in [&self.ws, &self.temp] {
            let _ = tokio::fs::remove_dir_all(d).await;
            if self.volume.is_none() || d == &self.temp {
                tokio::fs::create_dir_all(d).await?;
            }
        }
        for d in ["_cmds", "_scripts", "_pids"] {
            tokio::fs::create_dir_all(self.temp.join(d)).await?;
        }
        let event_path = self.temp.join("event.json");
        tokio::fs::write(&event_path, serde_json::to_vec_pretty(&super::event_payload(spec))?).await?;

        let (owner, name) = spec.slug();
        // A vault GITHUB_TOKEN (repo or project) wins over the server-wide --github-token.
        let token = spec
            .secrets
            .iter()
            .find(|(k, _)| k == "GITHUB_TOKEN")
            .map(|(_, v)| v.clone())
            .or_else(|| engine.gha.config.github_token.clone())
            .unwrap_or_default();
        self.log.mask(spec.secrets.iter().map(|(_, v)| v.as_str()).chain([token.as_str()]));
        let wf_name = spec.workflow.display_name(&spec.file);
        let arch = match std::env::consts::ARCH {
            "aarch64" => "ARM64",
            "x86_64" => "X64",
            other => other,
        };
        let ws = self.ws.display().to_string();
        let temp = self.temp.display().to_string();
        let ref_name = short_ref(&spec.git_ref).to_string();
        let ref_type = super::ref_type(&spec.git_ref);
        let workflow_ref = format!("{owner}/{name}/{}@{}", spec.file, spec.git_ref);
        self.github = json!({
            "action": "", "action_path": "", "action_ref": "", "action_repository": "",
            "actor": "moloko", "api_url": "https://api.github.com", "base_ref": "", "head_ref": "",
            "env": "", "path": "", "output": "", "state": "", "step_summary": "",
            "event": super::event_payload(spec), "event_name": spec.event,
            "event_path": event_path.display().to_string(),
            "graphql_url": "https://api.github.com/graphql", "job": self.inst.job_id,
            "ref": spec.git_ref, "ref_name": ref_name, "ref_protected": false, "ref_type": ref_type,
            "repository": format!("{owner}/{name}"), "repository_owner": owner,
            "repositoryUrl": format!("git://github.com/{owner}/{name}.git"),
            "retention_days": "90", "run_id": spec.run_id.to_string(),
            "run_number": spec.run_id.to_string(), "run_attempt": "1",
            "server_url": "https://github.com", "sha": spec.sha, "token": token,
            "triggering_actor": "moloko", "workflow": wf_name, "workflow_ref": workflow_ref,
            "workspace": ws,
        });
        self.runner = json!({
            "name": "moloko", "os": "Linux", "arch": arch, "temp": temp,
            "tool_cache": "/opt/hostedtoolcache", "debug": "", "environment": "self-hosted",
        });
        let mut secrets: Map<String, Value> = spec.secrets.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        secrets.insert("GITHUB_TOKEN".into(), Value::String(token.clone()));
        self.secrets = Value::Object(secrets);
        let ws_parent = self.ws.parent().map(|p| p.display().to_string()).unwrap_or_default();
        let registry = &engine.registry_host;
        let instance_name = crate::gha::matrix::instance_name(&self.inst.job_id, &self.inst.matrix);
        self.base_env = vec![
            ("CI", "true".into()),
            ("GITHUB_ACTIONS", "true".into()),
            ("GITHUB_ACTOR", "moloko".into()),
            ("GITHUB_API_URL", "https://api.github.com".into()),
            ("GITHUB_BASE_REF", String::new()),
            ("GITHUB_EVENT_NAME", spec.event.into()),
            ("GITHUB_EVENT_PATH", event_path.display().to_string()),
            ("GITHUB_GRAPHQL_URL", "https://api.github.com/graphql".into()),
            ("GITHUB_HEAD_REF", String::new()),
            ("GITHUB_JOB", self.inst.job_id.clone()),
            ("GITHUB_REF", spec.git_ref.clone()),
            ("GITHUB_REF_NAME", ref_name),
            ("GITHUB_REF_PROTECTED", "false".into()),
            ("GITHUB_REF_TYPE", ref_type.into()),
            ("GITHUB_REPOSITORY", format!("{owner}/{name}")),
            ("GITHUB_REPOSITORY_OWNER", owner.clone()),
            ("GITHUB_RETENTION_DAYS", "90".into()),
            ("GITHUB_RUN_ATTEMPT", "1".into()),
            ("GITHUB_RUN_ID", spec.run_id.to_string()),
            ("GITHUB_RUN_NUMBER", spec.run_id.to_string()),
            ("GITHUB_SERVER_URL", "https://github.com".into()),
            ("GITHUB_SHA", spec.sha.clone()),
            ("GITHUB_TRIGGERING_ACTOR", "moloko".into()),
            ("GITHUB_WORKFLOW", wf_name),
            ("GITHUB_WORKFLOW_REF", workflow_ref),
            ("GITHUB_WORKSPACE", ws),
            ("RUNNER_ARCH", arch.into()),
            ("RUNNER_NAME", "moloko".into()),
            ("RUNNER_OS", "Linux".into()),
            ("RUNNER_TEMP", temp),
            ("RUNNER_TOOL_CACHE", "/opt/hostedtoolcache".into()),
            ("RUNNER_WORKSPACE", ws_parent),
            ("ImageOS", "ubuntu22".into()),
            // Containers have no FUSE, so AppImages (linuxdeploy, used by Tauri) unpack instead of mounting.
            ("APPIMAGE_EXTRACT_AND_RUN", "1".into()),
            // The workspace is owned by a different uid than the container user.
            ("GIT_CONFIG_COUNT", "1".into()),
            ("GIT_CONFIG_KEY_0", "safe.directory".into()),
            ("GIT_CONFIG_VALUE_0", "*".into()),
            ("MOLOKO", "true".into()),
            ("MOLOKO_RUN_ID", spec.run_id.to_string()),
            ("MOLOKO_JOB", instance_name),
            ("MOLOKO_REF", spec.git_ref.clone()),
            ("MOLOKO_SHA", spec.sha.clone()),
            ("MOLOKO_REGISTRY", registry.clone()),
            ("MOLOKO_REGISTRY_USER", "moloko".into()),
            ("MOLOKO_REGISTRY_TOKEN", spec.registry_token.clone()),
            ("MOLOKO_IMAGE", spec.image.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        // env: workflow, then job (which may reference matrix etc.)
        let mut env = Vec::new();
        {
            let c = self.job_contexts(&[]);
            let ctx = self.ctx(&c, false);
            for (k, v) in &spec.workflow.env {
                set(&mut env, k, expr::interpolate_str(&expr::to_string(v), &ctx).with_context(|| format!("env.{k}"))?);
            }
        }
        if let Some(job_env) = &self.inst.job.env {
            let c = self.job_contexts(&env);
            let v = expr::interpolate_value(job_env, &self.ctx(&c, false)).context("jobs.<id>.env")?;
            match v {
                Value::Object(m) => {
                    for (k, v) in m {
                        set(&mut env, &k, expr::to_string(&v));
                    }
                }
                Value::Null => {}
                other => bail!("jobs.<id>.env must be a map, got {other}"),
            }
        }
        self.job_env = env;

        let (image, container_env, container_opts, container_vols) = (runner.image, runner.env, runner.opts, runner.vols);
        let c = self.job_contexts(&self.job_env);
        let ctx = self.ctx(&c, false);
        let services = self
            .inst
            .job
            .services
            .iter()
            .map(|(k, v)| Ok((k.clone(), expr::interpolate_value(v, &ctx)?)))
            .collect::<Result<Vec<_>>>()
            .context("services")?;

        if image == "host" {
            return self.setup_host(&services).await;
        }
        self.ensure_image(&image).await?;
        let _ = docker_out(&strs(&["rm", "-f", &self.container])).await;
        let mut args = strs(&["run", "-d", "--name", &self.container]);
        for (k, _) in &services {
            args.extend(["--add-host".into(), format!("{k}:127.0.0.1")]);
        }
        let actions = engine.gha.actions_dir.display().to_string();
        for (src, dst, mode) in [
            (self.volume.clone().unwrap_or_else(|| self.ws.display().to_string()), self.ws.display().to_string(), ""),
            (self.temp.display().to_string(), self.temp.display().to_string(), ""),
            (self.mirror.display().to_string(), self.mirror.display().to_string(), ""),
            (actions.clone(), actions, ":ro"),
            (DOCKER_SOCK.into(), DOCKER_SOCK.into(), ""),
        ] {
            args.extend(["-v".into(), format!("{src}:{dst}{mode}")]);
        }
        for v in container_vols {
            args.extend(["-v".into(), v]);
        }
        if image == super::WINDOWS_CROSS_IMAGE {
            let xwin = engine.gha.xwin_dir();
            tokio::fs::create_dir_all(&xwin).await?;
            let xwin = xwin.display().to_string();
            args.extend(["-v".into(), format!("{xwin}:{xwin}")]);
            set(&mut self.base_env, "XWIN_CACHE_DIR", xwin);
        }
        for (k, v) in &container_env {
            args.extend(["-e".into(), format!("{k}={}", expr::to_string(v))]);
        }
        args.extend(split_words(&container_opts));
        args.extend([
            "-w".into(),
            self.ws.display().to_string(),
            "--entrypoint".into(),
            "tail".into(),
            image.clone(),
            "-f".into(),
            "/dev/null".into(),
        ]);
        self.line(&format!("▶ runner: {image}"));
        docker_out(&args).await.context("starting job container")?;
        self.container_started = true;
        if self.volume.is_some() {
            let (container, ws) = (self.container.clone(), self.ws.display().to_string());
            self.hasher = Some(Box::new(move |patterns: &[String]| volume_hash_files(&container, &ws, patterns)));
        }
        let info = docker_out(&[
            "exec".into(),
            self.container.clone(),
            "sh".into(),
            "-c".into(),
            "printf '%s\\n%s' \"$PATH\" \"$HOME\"".into(),
        ])
        .await
        .context("job container has no `sh`")?;
        let mut it = info.lines();
        self.base_path = it.next().unwrap_or("").to_string();
        if let Some(h) = it.next().filter(|h| !h.is_empty()) {
            self.home = h.to_string();
        }

        for (key, svc) in services {
            self.start_service(&key, &svc).await?;
        }
        Ok(())
    }

    /// Pick the job's runner from `container:` or `runs-on` (job-level contexts only).
    fn resolve_runner(&self) -> Result<RunnerSpec> {
        let spec = &self.inst.spec;
        let contexts = super::job_level_contexts(
            spec,
            &self.inst.needs,
            &[("matrix", Value::Object(self.inst.matrix.clone())), ("strategy", self.inst.strategy.clone())],
        );
        let ctx = Ctx { contexts: &contexts, status: StatusCtx::default(), workspace: None, hasher: None };
        let container = self
            .inst
            .job
            .container
            .as_ref()
            .map(|v| expr::interpolate_value(v, &ctx))
            .transpose()
            .context("jobs.<id>.container")?;
        match &container {
            Some(Value::String(s)) if !s.is_empty() => {
                return Ok(RunnerSpec { image: s.clone(), env: Map::new(), opts: String::new(), vols: vec![], note: None });
            }
            Some(Value::Object(o)) if o.get("image").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) => {
                return Ok(RunnerSpec {
                    image: o["image"].as_str().unwrap().to_string(),
                    env: o.get("env").and_then(Value::as_object).cloned().unwrap_or_default(),
                    opts: o.get("options").map(expr::to_string).unwrap_or_default(),
                    vols: o
                        .get("volumes")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().map(expr::to_string).collect())
                        .unwrap_or_default(),
                    note: None,
                });
            }
            _ => {}
        }
        let runs_on = self
            .inst
            .job
            .runs_on
            .as_ref()
            .map(|v| expr::interpolate_value(v, &ctx))
            .transpose()
            .context("runs-on")?
            .ok_or_else(|| anyhow!("job has no runs-on"))?;
        let labels: Vec<String> = match &runs_on {
            Value::Array(a) => a.iter().map(expr::to_string).collect(),
            Value::Object(o) => match o.get("labels") {
                Some(Value::Array(a)) => a.iter().map(expr::to_string).collect(),
                Some(v) => vec![expr::to_string(v)],
                None => vec![],
            },
            v => vec![expr::to_string(v)],
        };
        let (image, note) = self.inst.engine.gha.config.image_for(&labels)?;
        Ok(RunnerSpec { image, env: Map::new(), opts: String::new(), vols: vec![], note })
    }

    /// Host mode: steps run as processes on the build machine, like a GitHub macOS runner.
    /// HOME and the tool cache are moloko's own (persistent), so jobs don't touch the user's home.
    async fn setup_host(&mut self, services: &[(String, Value)]) -> Result<()> {
        if self.inst.job.container.is_some() {
            bail!("`container:` needs a Linux container runner; this job runs on the host");
        }
        if !services.is_empty() {
            bail!("`services:` need a Linux container runner; this job runs on the host (GitHub's macOS runners don't support them either)");
        }
        let root = self.inst.engine.gha.actions_dir.parent().unwrap_or(Path::new(".")).join("host");
        let home = root.join("home");
        let tools = root.join("toolcache");
        for d in [&home, &tools] {
            tokio::fs::create_dir_all(d).await?;
        }
        self.host = true;
        self.home = home.display().to_string();
        let os = if cfg!(target_os = "macos") { "macOS" } else { "Linux" };
        let image_os = if cfg!(target_os = "macos") { "macos15" } else { "ubuntu24" };
        let tools = tools.display().to_string();
        self.runner["os"] = Value::String(os.into());
        self.runner["tool_cache"] = Value::String(tools.clone());
        for (k, v) in [
            ("RUNNER_OS", os.to_string()),
            ("RUNNER_TOOL_CACHE", tools.clone()),
            ("AGENT_TOOLSDIRECTORY", tools),
            ("ImageOS", image_os.to_string()),
            ("HOME", self.home.clone()),
        ] {
            set(&mut self.base_env, k, v);
        }
        self.base_path = std::env::var("PATH").unwrap_or_default();
        for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            if !self.base_path.split(':').any(|p| p == extra) {
                self.base_path = format!("{}:{extra}", self.base_path).trim_start_matches(':').to_string();
            }
        }
        set(&mut self.base_env, "PATH", self.base_path.clone());
        self.line(&format!("▶ runner: this {os} host ({})", std::env::consts::ARCH));
        Ok(())
    }

    /// Run a short command in the job's environment (container or host), cwd = workspace.
    async fn job_out(&self, argv: &[String]) -> Result<String> {
        if !self.host {
            let mut args = vec!["exec".into(), "-w".into(), self.ws.display().to_string(), self.container.clone()];
            args.extend(argv.iter().cloned());
            return docker_out(&args).await;
        }
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.ws)
            .envs(self.base_env.iter().cloned())
            .output()
            .await
            .with_context(|| format!("failed to run {}", argv[0]))?;
        if !out.status.success() {
            bail!("{}: {}", argv[0], String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// `cp` on the host, preferring copy-on-write clones on macOS.
    async fn host_cp(src: &str, dst: &str) -> Result<()> {
        let flags: &[&[&str]] = if cfg!(target_os = "macos") { &[&["-cR"], &["-R"]] } else { &[&["-R", "--reflink=auto"], &["-R"]] };
        let mut last = String::new();
        for f in flags {
            let out = Command::new("cp").args(*f).arg(src).arg(dst).output().await?;
            if out.status.success() {
                return Ok(());
            }
            last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        }
        bail!("cp {src} {dst}: {last}")
    }

    /// Copy a host path into the job at `dst` (a directory's contents go into `dst`).
    async fn copy_in(&self, src: &Path, dst: &str) -> Result<()> {
        let parent = Path::new(dst).parent().map(|p| p.display().to_string()).unwrap_or("/".into());
        self.job_out(&strs(&["mkdir", "-p", &parent])).await?;
        let src_arg = if src.is_dir() { format!("{}/.", src.display()) } else { src.display().to_string() };
        if self.host {
            if src.is_dir() {
                tokio::fs::create_dir_all(dst).await?;
            }
            Self::host_cp(&src_arg, dst).await
        } else {
            docker_out(&["cp".into(), src_arg, format!("{}:{dst}", self.container)]).await.map(|_| ())
        }
    }

    /// Copy a job path (`dir/.` for a directory's contents) to the host, like `docker cp`.
    async fn copy_out(&self, src: &str, dst: &Path) -> Result<()> {
        if self.host {
            Self::host_cp(src, &dst.display().to_string()).await
        } else {
            docker_out(&["cp".into(), format!("{}:{src}", self.container), dst.display().to_string()]).await.map(|_| ())
        }
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        super::setup::ensure_image(&self.inst.engine.gha, image, &self.temp, &|l| self.line(l), &self.inst.cancel).await
    }

    async fn start_service(&mut self, key: &str, svc: &Value) -> Result<()> {
        let (image, env, opts, vols) = match svc {
            Value::String(s) => (s.clone(), Map::new(), String::new(), vec![]),
            Value::Object(o) => (
                o.get("image").map(expr::to_string).unwrap_or_default(),
                o.get("env").and_then(Value::as_object).cloned().unwrap_or_default(),
                o.get("options").map(expr::to_string).unwrap_or_default(),
                o.get("volumes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(expr::to_string).collect::<Vec<_>>())
                    .unwrap_or_default(),
            ),
            _ => bail!("services.{key} must be an image or a map"),
        };
        if image.is_empty() {
            self.line(&format!("◦ service {key}: no image, skipped"));
            return Ok(());
        }
        self.ensure_image(&image).await?;
        let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let name = format!("moloko-svc-{}-{safe}", self.inst.row);
        let _ = docker_out(&strs(&["rm", "-f", &name])).await;
        // Services share the job container's network namespace: `localhost:<port>` and
        // `<service>:<port>` (via --add-host) both reach them, like on a GitHub-hosted runner.
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--network".into(),
            format!("container:{}", self.container),
        ];
        for (k, v) in &env {
            args.extend(["-e".into(), format!("{k}={}", expr::to_string(v))]);
        }
        for v in vols {
            args.extend(["-v".into(), v]);
        }
        args.extend(split_words(&opts));
        args.push(image.clone());
        docker_out(&args).await.with_context(|| format!("starting service {key}"))?;
        self.services.push((key.to_string(), name.clone()));
        self.line(&format!("▶ service {key}: {image}"));
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let state = docker_out(&[
                "inspect".into(),
                "-f".into(),
                "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}".into(),
                name.clone(),
            ])
            .await?;
            match state.split_once(' ') {
                Some(("running", "none" | "healthy")) => break,
                Some((s, _)) if s != "running" && s != "created" => {
                    bail!("service {key} stopped ({state}); see `docker logs {name}`")
                }
                Some((_, "unhealthy")) => bail!("service {key} is unhealthy"),
                _ => {}
            }
            if Instant::now() > deadline {
                bail!("service {key} didn't become healthy within 5 minutes");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = self.inst.cancel.cancelled() => bail!("cancelled"),
            }
        }
        self.line(&format!("  service {key} is ready"));
        Ok(())
    }

    async fn cleanup(&mut self) {
        if self.container_started {
            if cfg!(target_os = "linux") {
                // Files created as root inside the container must stay removable by us.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                let _ = docker_out(&[
                    "exec".into(),
                    self.container.clone(),
                    "chown".into(),
                    "-R".into(),
                    format!("{uid}:{gid}"),
                    self.ws.display().to_string(),
                    self.temp.display().to_string(),
                    self.mirror.join("worktrees").display().to_string(),
                ])
                .await;
            }
            let mut args = strs(&["rm", "-f", &self.container]);
            args.extend(self.services.iter().map(|(_, n)| n.clone()));
            let _ = docker_out(&args).await;
        }
        for wt in std::mem::take(&mut self.worktrees).into_iter().rev() {
            self.inst.engine.git.remove(self.inst.spec.repo.id, &wt).await;
        }
        for d in [&self.ws, &self.temp] {
            let _ = tokio::fs::remove_dir_all(d).await;
        }
        if let Some(v) = self.volume.take() {
            let _ = docker_out(&strs(&["volume", "rm", "-f", &v])).await;
        }
    }

    // ---------- steps ----------

    fn run_steps<'s>(&'s mut self, steps: &'s [Step], scope: &'s mut Scope) -> Pin<Box<dyn Future<Output = ()> + Send + 's>>
    where
        'a: 's,
    {
        Box::pin(async move {
            for (i, step) in steps.iter().enumerate() {
                self.run_one(step, i, scope).await;
            }
        })
    }

    async fn run_one(&mut self, step: &Step, idx: usize, scope: &mut Scope) {
        let contexts = self.contexts(scope, &[]);
        let ctx = self.ctx(&contexts, scope.failed);
        let name = match &step.name {
            Some(n) => expr::interpolate_str(n, &ctx).unwrap_or_else(|_| n.clone()),
            None => match (&step.uses, step.run_script()) {
                (Some(u), _) => format!("Run {u}"),
                (_, Some(r)) => format!("Run {}", r.lines().next().unwrap_or("").trim()),
                _ => "step".into(),
            },
        };
        let record = |scope: &mut Scope, outcome: &str, conclusion: &str, outputs: Map<String, Value>| {
            if let Some(id) = &step.id {
                scope.steps.insert(
                    id.clone(),
                    json!({ "outputs": outputs, "outcome": outcome, "conclusion": conclusion }),
                );
            }
        };
        if self.inst.cancel.is_cancelled() {
            self.line(&format!("◦ {name} (cancelled)"));
            record(scope, "cancelled", "cancelled", Map::new());
            return;
        }
        match expr::eval_if(&cond_str(step.cond.as_ref()), &ctx) {
            Ok(true) => {}
            Ok(false) => {
                self.line(&format!("◦ {name} (skipped)"));
                record(scope, "skipped", "skipped", Map::new());
                return;
            }
            Err(e) => {
                self.line(&format!("✗ {name}: if: {e:#}"));
                scope.failed = true;
                record(scope, "failure", "failure", Map::new());
                return;
            }
        }
        let mut step_env = Vec::new();
        for (k, v) in &step.env {
            match expr::interpolate_str(&expr::to_string(v), &ctx) {
                Ok(v) => set(&mut step_env, k, v),
                Err(e) => {
                    self.line(&format!("✗ {name}: env.{k}: {e:#}"));
                    scope.failed = true;
                    record(scope, "failure", "failure", Map::new());
                    return;
                }
            }
        }
        let continue_on_error = step
            .continue_on_error
            .as_ref()
            .and_then(|v| expr::interpolate_value(v, &ctx).ok())
            .is_some_and(|v| expr::truthy(&v));
        let timeout = step
            .timeout_minutes
            .as_ref()
            .and_then(|v| expr::interpolate_value(v, &ctx).ok())
            .and_then(|v| v.as_f64().or_else(|| expr::to_string(&v).parse().ok()));
        drop(contexts);

        self.line(&format!("▶ {name}"));
        let step_cancel = self.inst.cancel.child_token();
        let timed_out = Arc::new(AtomicBool::new(false));
        let timer = timeout.map(|t| {
            let (flag, c) = (timed_out.clone(), step_cancel.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(t * 60.0)).await;
                flag.store(true, Ordering::SeqCst);
                c.cancel();
            })
        });
        let res = if step.run.is_some() {
            self.run_script(step, scope, &step_env, &step_cancel).await
        } else {
            self.run_uses(step, idx, scope, &step_env, &step_cancel).await
        };
        if let Some(t) = timer {
            t.abort();
        }
        let (mut outcome, outputs) = res.unwrap_or_else(|e| {
            self.line(&format!("✗ {e:#}"));
            ("failure", Map::new())
        });
        if outcome == "cancelled" && timed_out.load(Ordering::SeqCst) {
            self.line(&format!("✗ step timed out after {} minutes", timeout.unwrap_or_default()));
            outcome = "failure";
        }
        let conclusion = if outcome == "failure" && continue_on_error {
            self.line("⚠ step failed, continuing (continue-on-error)");
            "success"
        } else {
            outcome
        };
        if conclusion == "failure" {
            scope.failed = true;
        }
        record(scope, outcome, conclusion, outputs);
    }

    async fn run_post(&mut self, scope: &mut Scope) {
        while let Some(p) = self.post.pop() {
            if self.inst.cancel.is_cancelled() {
                break;
            }
            let contexts = self.contexts(scope, &[]);
            match expr::eval_if(&p.cond, &self.ctx(&contexts, scope.failed)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    self.line(&format!("✗ {}: {e:#}", p.name));
                    continue;
                }
            }
            self.line(&format!("▶ {}", p.name));
            let cancel = self.inst.cancel.child_token();
            let res = match p.kind {
                PostKind::Process(argv) => {
                    let ws = self.ws.clone();
                    self.exec_process(&argv, scope, &p.env, &ws, &cancel).await
                }
                PostKind::Container { image, entrypoint, args } => {
                    self.run_container(&image, entrypoint, args, scope, &p.env, &cancel).await
                }
                PostKind::CacheSave { key, paths } => self.cache_save(&key, &paths).await.map(|_| ("success", Map::new())),
            };
            match res {
                Ok(("failure", _)) => scope.failed = true,
                Err(e) => {
                    self.line(&format!("✗ {e:#}"));
                    scope.failed = true;
                }
                _ => {}
            }
        }
    }

    // ---------- processes ----------

    fn new_files(&self) -> Result<Files> {
        let n = self.next();
        let dir = self.temp.join("_cmds");
        let f = |k: &str| dir.join(format!("{k}_{n}"));
        let files = Files {
            env: f("env"),
            output: f("output"),
            path: f("path"),
            state: f("state"),
            summary: f("step_summary"),
        };
        for p in [&files.env, &files.output, &files.path, &files.state, &files.summary] {
            std::fs::write(p, "")?;
        }
        Ok(files)
    }

    /// Read back the environment files a step wrote. Returns (outputs, state).
    fn apply_files(&mut self, files: &Files) -> (Map<String, Value>, Vec<(String, String)>) {
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        for (k, v) in commands::parse_kv_file(&read(&files.env)) {
            set(&mut self.file_env, &k, v);
        }
        for p in read(&files.path).lines().map(str::trim).filter(|l| !l.is_empty()) {
            self.path_add.insert(0, p.to_string());
        }
        let cap = std::mem::take(&mut *self.capture.lock().unwrap());
        let mut outputs = cap.outputs;
        for (k, v) in commands::parse_kv_file(&read(&files.output)) {
            outputs.insert(k, Value::String(v));
        }
        let mut state = cap.state;
        state.extend(commands::parse_kv_file(&read(&files.state)));
        (outputs, state)
    }

    fn exec_env(&self, scope: &Scope, extra: &[(String, String)], files: &Files) -> Vec<(String, String)> {
        let mut env = merged(&[&self.base_env, &scope.env, &self.file_env, extra]);
        for (k, p) in [
            ("GITHUB_ENV", &files.env),
            ("GITHUB_OUTPUT", &files.output),
            ("GITHUB_PATH", &files.path),
            ("GITHUB_STATE", &files.state),
            ("GITHUB_STEP_SUMMARY", &files.summary),
        ] {
            set(&mut env, k, p.display().to_string());
        }
        if let Some(p) = &scope.action_path {
            set(&mut env, "GITHUB_ACTION_PATH", p.display().to_string());
        }
        // $GITHUB_PATH entries go in front, unless a step set PATH itself.
        let user_path = [&scope.env[..], &self.file_env[..], extra].iter().any(|l| l.iter().any(|(k, _)| k == "PATH"));
        if !self.path_add.is_empty() && !user_path {
            set(&mut env, "PATH", format!("{}:{}", self.path_add.join(":"), self.base_path));
        }
        env
    }

    /// `docker exec` a command in the job container. Killing the docker client doesn't stop
    /// the process inside, so the wrapper records its pid for cancellation.
    async fn exec_in(&self, argv: &[String], env: &[(String, String)], workdir: &Path, cancel: &CancellationToken) -> Result<StepResult> {
        if self.host {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(workdir).envs(env.iter().cloned());
            return stream(cmd, &|l| self.handle_line(l), cancel).await;
        }
        let pidfile = self.temp.join("_pids").join(self.next().to_string());
        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-w").arg(workdir);
        // `-e NAME` takes the value from docker's own environment, so values (secrets) never
        // appear in a process list. Shells drop variables whose names aren't identifiers
        // (e.g. `INPUT_GITHUB-TOKEN`) when they exec, so those travel under an alias and
        // are renamed by `env` inside the container.
        let valid = |k: &str| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let mut renames = String::new();
        for (i, (k, v)) in env.iter().enumerate() {
            if valid(k) {
                cmd.env(k, v).arg("-e").arg(k);
            } else {
                let alias = format!("MOLOKO_ENV_{i}");
                cmd.env(&alias, v).arg("-e").arg(&alias);
                renames.push_str(&format!(" {}=\"${alias}\"", sh_quote(k)));
            }
        }
        let rename = if renames.is_empty() { String::new() } else { format!("env{renames} ") };
        let script = format!("echo $$ > {}; exec {rename}\"$@\"", sh_quote(&pidfile.display().to_string()));
        cmd.arg(&self.container).args(["sh", "-c", &script, "moloko"]).args(argv);
        let r = stream(cmd, &|l| self.handle_line(l), cancel).await?;
        if matches!(r, StepResult::Cancelled) {
            let _ = docker_out(&[
                "exec".into(),
                self.container.clone(),
                "sh".into(),
                "-c".into(),
                format!("kill -9 \"$(cat {})\" 2>/dev/null", sh_quote(&pidfile.display().to_string())),
            ])
            .await;
        }
        Ok(r)
    }

    /// Run a process with fresh environment files; returns (outcome, outputs).
    async fn exec_process(&mut self, argv: &[String], scope: &Scope, extra: &[(String, String)], workdir: &Path, cancel: &CancellationToken) -> Result<Outcome> {
        let (outcome, outputs, _) = self.exec_process_state(argv, scope, extra, workdir, cancel).await?;
        Ok((outcome, outputs))
    }

    async fn exec_process_state(
        &mut self,
        argv: &[String],
        scope: &Scope,
        extra: &[(String, String)],
        workdir: &Path,
        cancel: &CancellationToken,
    ) -> Result<(&'static str, Map<String, Value>, Vec<(String, String)>)> {
        let files = self.new_files()?;
        let env = self.exec_env(scope, extra, &files);
        let r = self.exec_in(argv, &env, workdir, cancel).await?;
        let (outputs, state) = self.apply_files(&files);
        let outcome = match r {
            StepResult::Exited(0) => "success",
            StepResult::Exited(127) => {
                let place = if self.host { "on this host's PATH" } else { "in the job container" };
                self.line(&format!("✗ `{}` not found {place} (exit code 127)", argv[0]));
                "failure"
            }
            StepResult::Exited(c) => {
                self.line(&format!("✗ exited with code {c}"));
                "failure"
            }
            StepResult::Cancelled => "cancelled",
        };
        Ok((outcome, outputs, state))
    }

    fn resolve_dir(&self, dir: Option<&str>) -> PathBuf {
        match dir.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) if d.starts_with('/') => PathBuf::from(d),
            Some(d) => self.ws.join(d),
            None => self.ws.clone(),
        }
    }

    async fn run_script(&mut self, step: &Step, scope: &Scope, step_env: &[(String, String)], cancel: &CancellationToken) -> Result<Outcome> {
        let contexts = self.contexts(scope, step_env);
        let ctx = self.ctx(&contexts, scope.failed);
        let script = expr::interpolate_str(&step.run_script().unwrap_or_default(), &ctx)?;
        let job = &self.inst.job;
        let wf = &self.inst.spec.workflow;
        let shell = step.shell.clone().or_else(|| {
            (!scope.composite)
                .then(|| job.defaults.run.shell.clone().or_else(|| wf.defaults.run.shell.clone()))
                .flatten()
        });
        let wd = step
            .working_directory
            .clone()
            .or_else(|| {
                (!scope.composite)
                    .then(|| {
                        job.defaults
                            .run
                            .working_directory
                            .clone()
                            .or_else(|| wf.defaults.run.working_directory.clone())
                    })
                    .flatten()
            })
            .map(|d| expr::interpolate_str(&d, &ctx))
            .transpose()?;
        let workdir = self.resolve_dir(wd.as_deref());
        let ext = match shell.as_deref() {
            Some("python") => "py",
            Some("pwsh" | "powershell") => "ps1",
            _ => "sh",
        };
        let file = self.temp.join("_scripts").join(format!("{}.{ext}", self.next()));
        std::fs::write(&file, script)?;
        let argv = shell_argv(shell.as_deref(), &file.display().to_string())?;
        drop(contexts);
        self.exec_process(&argv, scope, step_env, &workdir, cancel).await
    }

    async fn run_uses(&mut self, step: &Step, idx: usize, scope: &mut Scope, step_env: &[(String, String)], cancel: &CancellationToken) -> Result<Outcome> {
        let contexts = self.contexts(scope, step_env);
        let ctx = self.ctx(&contexts, scope.failed);
        let uses = expr::interpolate_str(step.uses.as_deref().unwrap_or(""), &ctx)?;
        let mut with: Vec<(String, String)> = Vec::new();
        for (k, v) in &step.with {
            with.push((k.clone(), expr::interpolate_str(&expr::to_string(v), &ctx).with_context(|| format!("with.{k}"))?));
        }
        drop(contexts);
        match parse_uses(&uses)? {
            Uses::Docker(image) => {
                let get = |k: &str| with.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
                let args = get("args").map(|a| split_words(&a)).unwrap_or_default();
                let entrypoint = get("entrypoint").filter(|e| !e.is_empty());
                let mut env = step_env.to_vec();
                for (k, v) in &with {
                    set(&mut env, &commands::input_env_name(k), v.clone());
                }
                self.ensure_image(&image).await?;
                self.run_container(&image, entrypoint, args, scope, &env, cancel).await
            }
            Uses::Local(path) => {
                let dir = self.ws.join(path.trim_start_matches("./"));
                // A volume workspace isn't visible here: read the action from a copy.
                let meta_dir = if self.volume.is_some() {
                    let copy = self.temp.join("_local_actions").join(self.next().to_string());
                    tokio::fs::create_dir_all(copy.parent().unwrap()).await?;
                    self.copy_out(&dir.display().to_string(), &copy).await.with_context(|| format!("reading local action {path}"))?;
                    copy
                } else {
                    dir.clone()
                };
                self.run_action(&dir, &meta_dir, None, true, step, idx, scope, step_env, &with, cancel).await
            }
            Uses::Remote { owner, repo, path, git_ref } => {
                let full = match &path {
                    Some(p) => format!("{owner}/{repo}/{p}"),
                    None => format!("{owner}/{repo}"),
                }
                .to_lowercase();
                let get = |k: &str| {
                    with.iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(k))
                        .map(|(_, v)| v.clone())
                        .filter(|v| !v.is_empty())
                };
                match full.as_str() {
                    "actions/checkout" if get("repository").is_none_or(|r| r.eq_ignore_ascii_case(self.github["repository"].as_str().unwrap_or_default())) => {
                        return self.native_checkout(&with, step_env, cancel).await;
                    }
                    "actions/cache" | "actions/cache/restore" | "actions/cache/save" => {
                        return self.native_cache(&full, &with).await;
                    }
                    "actions/upload-artifact" => return self.native_upload(&with).await,
                    "actions/download-artifact" => return self.native_download(&with).await,
                    _ => {}
                }
                let root = self.fetch_action(&owner, &repo, &git_ref).await?;
                let dir = match &path {
                    Some(p) => root.join(p),
                    None => root,
                };
                let repo_ref = (format!("{owner}/{repo}"), git_ref);
                self.run_action(&dir, &dir, Some(repo_ref), false, step, idx, scope, step_env, &with, cancel).await
            }
        }
    }

    async fn fetch_action(&self, owner: &str, repo: &str, git_ref: &str) -> Result<PathBuf> {
        let shared = &self.inst.engine.gha;
        let safe = |s: &str| s.replace(['/', '\\'], "%2F");
        let dir = shared.actions_dir.join(safe(owner)).join(format!("{}@{}", safe(repo), safe(git_ref)));
        let lock = shared.lock(&dir.display().to_string());
        let _g = lock.lock().await;
        if dir.join(".moloko-complete").exists() {
            return Ok(dir);
        }
        self.line(&format!("▶ fetching action {owner}/{repo}@{git_ref}"));
        let tmp = dir.with_extension("tmp");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(&tmp).await?;
        let url = format!("https://github.com/{owner}/{repo}");
        let mut auth = Vec::new();
        if let Some(t) = &shared.config.github_token {
            use base64::Engine as _;
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"));
            auth = vec![
                "-c".to_string(),
                format!("http.https://github.com/.extraheader=AUTHORIZATION: basic {basic}"),
            ];
        }
        let tmp_s = tmp.display().to_string();
        let run = |args: Vec<String>| async move {
            let out = Command::new("git")
                .args(&args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .await?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            anyhow::Ok(())
        };
        run(strs(&["init", "-q", &tmp_s])).await?;
        let mut fetch = auth.clone();
        fetch.extend(strs(&["-C", &tmp_s, "fetch", "-q", "--depth", "1", &url, git_ref]));
        run(fetch)
            .await
            .with_context(|| format!("fetching {owner}/{repo}@{git_ref} (private actions need --github-token)"))?;
        run(strs(&["-C", &tmp_s, "-c", "advice.detachedHead=false", "checkout", "-q", "FETCH_HEAD"])).await?;
        let _ = tokio::fs::remove_dir_all(tmp.join(".git")).await;
        tokio::fs::write(tmp.join(".moloko-complete"), "").await?;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(dir.parent().unwrap()).await?;
        tokio::fs::rename(&tmp, &dir).await?;
        Ok(dir)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_action(
        &mut self,
        dir: &Path,
        meta_dir: &Path,
        repo_ref: Option<(String, String)>,
        local: bool,
        step: &Step,
        idx: usize,
        scope: &mut Scope,
        step_env: &[(String, String)],
        with: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let meta = ["action.yml", "action.yaml"]
            .iter()
            .map(|f| meta_dir.join(f))
            .find(|p| p.exists())
            .ok_or_else(|| anyhow!("no action.yml in {}", dir.display()))?;
        let action = Action::parse(&std::fs::read_to_string(&meta)?)?;
        let action_name = action.name.clone().unwrap_or_else(|| step.uses.clone().unwrap_or_default());

        // Inputs: `with:` values, else the action's defaults (which may use expressions).
        let contexts = self.contexts(scope, step_env);
        let ctx = self.ctx(&contexts, scope.failed);
        let mut inputs: Vec<(String, String)> = Vec::new();
        for (name, spec) in &action.inputs {
            let given = with.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
            let value = match given {
                Some(v) => v,
                None => match spec.get("default") {
                    Some(d) => {
                        let d = expr::to_string(d);
                        if d.replace(' ', "").contains("github.token") && self.secrets["GITHUB_TOKEN"].as_str().unwrap_or("").is_empty() {
                            self.line(&format!(
                                "⚠ input {name} defaults to github.token, but no token is configured: add a GITHUB_TOKEN secret to the repo or project, or pass `{name}:` explicitly"
                            ));
                        }
                        expr::interpolate_str(&d, &ctx).with_context(|| format!("default of input {name}"))?
                    }
                    None => {
                        if spec.get("required").is_some_and(expr::truthy) {
                            self.line(&format!("⚠ input {name} is required but not set"));
                        }
                        String::new()
                    }
                },
            };
            inputs.push((name.clone(), value));
        }
        for (k, v) in with {
            if !inputs.iter().any(|(n, _)| n.eq_ignore_ascii_case(k)) {
                inputs.push((k.clone(), v.clone()));
            }
        }
        drop(contexts);
        let mut env: Vec<(String, String)> = step_env.to_vec();
        for (k, v) in &inputs {
            set(&mut env, &commands::input_env_name(k), v.clone());
        }
        let (action_repo, action_ref) = repo_ref.unwrap_or_default();
        for (k, v) in [
            ("GITHUB_ACTION_PATH", dir.display().to_string()),
            ("GITHUB_ACTION_REPOSITORY", action_repo),
            ("GITHUB_ACTION_REF", action_ref),
            ("GITHUB_ACTION", step.id.clone().unwrap_or_else(|| format!("__run_{idx}"))),
        ] {
            set(&mut env, k, v);
        }

        let using = action.runs.using.to_lowercase();
        let runs = &action.runs;
        if using.starts_with("node") {
            let ws = self.ws.clone();
            let mut state: Vec<(String, String)> = Vec::new();
            let with_state = |env: &[(String, String)], state: &[(String, String)]| {
                let mut e = env.to_vec();
                for (k, v) in state {
                    set(&mut e, &format!("STATE_{k}"), v.clone());
                }
                e
            };
            if let Some(pre) = &runs.pre {
                let contexts = self.contexts(scope, step_env);
                if expr::eval_if(runs.pre_if.as_deref().unwrap_or("always()"), &self.ctx(&contexts, scope.failed))? {
                    let argv = vec!["node".to_string(), dir.join(pre).display().to_string()];
                    let (o, _, s) = self.exec_process_state(&argv, scope, &env, &ws, cancel).await?;
                    state.extend(s);
                    if o != "success" {
                        return Ok((o, Map::new()));
                    }
                }
            }
            let main = runs.main.as_ref().ok_or_else(|| anyhow!("node action has no runs.main"))?;
            let argv = vec!["node".to_string(), dir.join(main).display().to_string()];
            let (outcome, outputs, s) = self.exec_process_state(&argv, scope, &with_state(&env, &state), &ws, cancel).await?;
            state.extend(s);
            if let Some(post) = &runs.post {
                self.post.push(PostStep {
                    name: format!("Post {action_name}"),
                    cond: runs.post_if.clone().unwrap_or_else(|| "always()".into()),
                    kind: PostKind::Process(vec!["node".into(), dir.join(post).display().to_string()]),
                    env: merged(&[&scope.env, &with_state(&env, &state)]),
                });
            }
            Ok((outcome, outputs))
        } else if using == "docker" {
            let image = self.action_image(meta_dir, runs.image.as_deref().unwrap_or(""), local).await?;
            let mut contexts = self.contexts(scope, step_env);
            contexts.insert(
                "inputs".into(),
                Value::Object(inputs.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()),
            );
            let ctx = self.ctx(&contexts, scope.failed);
            let args = runs
                .args
                .iter()
                .map(|a| expr::interpolate_str(&expr::to_string(a), &ctx))
                .collect::<Result<Vec<_>>>()?;
            for (k, v) in &runs.env {
                set(&mut env, k, expr::interpolate_str(&expr::to_string(v), &ctx)?);
            }
            drop(contexts);
            if let Some(pre) = &runs.pre_entrypoint {
                let (o, _) = self.run_container(&image, Some(pre.clone()), args.clone(), scope, &env, cancel).await?;
                if o != "success" {
                    return Ok((o, Map::new()));
                }
            }
            let r = self.run_container(&image, runs.entrypoint.clone(), args.clone(), scope, &env, cancel).await?;
            if let Some(post) = &runs.post_entrypoint {
                self.post.push(PostStep {
                    name: format!("Post {action_name}"),
                    cond: runs.post_if.clone().unwrap_or_else(|| "always()".into()),
                    kind: PostKind::Container { image, entrypoint: Some(post.clone()), args },
                    env: merged(&[&scope.env, &env]),
                });
            }
            Ok(r)
        } else if using == "composite" {
            if scope.depth >= MAX_COMPOSITE_DEPTH {
                bail!("composite actions nested more than {MAX_COMPOSITE_DEPTH} levels deep");
            }
            let mut nested = Scope {
                steps: Map::new(),
                inputs: inputs.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect(),
                env: merged(&[&scope.env, step_env]),
                action_path: Some(dir.to_path_buf()),
                failed: false,
                depth: scope.depth + 1,
                composite: true,
            };
            let steps = runs.steps.clone();
            self.run_steps(&steps, &mut nested).await;
            let contexts = self.contexts(&nested, &[]);
            let ctx = self.ctx(&contexts, nested.failed);
            let mut outputs = Map::new();
            for (k, spec) in &action.outputs {
                if let Some(v) = spec.get("value") {
                    outputs.insert(k.clone(), Value::String(expr::interpolate_str(&expr::to_string(v), &ctx)?));
                }
            }
            let outcome = if self.inst.cancel.is_cancelled() {
                "cancelled"
            } else if nested.failed {
                "failure"
            } else {
                "success"
            };
            Ok((outcome, outputs))
        } else {
            bail!("action {action_name}: runs.using {:?} isn't supported", runs.using)
        }
    }

    async fn action_image(&self, dir: &Path, image: &str, local: bool) -> Result<String> {
        if let Some(img) = image.strip_prefix("docker://") {
            self.ensure_image(img).await?;
            return Ok(img.to_string());
        }
        let dockerfile = dir.join(if image.is_empty() { "Dockerfile" } else { image });
        let tag = format!(
            "moloko-action-{}",
            &hex::encode(Sha256::digest(dir.display().to_string().as_bytes()))[..16]
        );
        if !local && docker_out(&strs(&["image", "inspect", &tag])).await.is_ok() {
            return Ok(tag);
        }
        self.line(&format!("▶ building action image from {}", dockerfile.display()));
        let mut cmd = Command::new("docker");
        cmd.args(["build", "-q", "-t", &tag, "-f"]).arg(&dockerfile).arg(dockerfile.parent().unwrap_or(dir));
        match stream(cmd, &|l| self.line(l), &self.inst.cancel).await? {
            StepResult::Exited(0) => Ok(tag),
            StepResult::Exited(c) => bail!("docker build failed with exit code {c}"),
            StepResult::Cancelled => bail!("cancelled"),
        }
    }

    /// Run a docker action's container next to the job container (same network namespace).
    async fn run_container(
        &mut self,
        image: &str,
        entrypoint: Option<String>,
        args: Vec<String>,
        scope: &Scope,
        extra: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let files = self.new_files()?;
        let mut env = self.exec_env(scope, extra, &files);
        env.retain(|(k, _)| k != "PATH");
        set(&mut env, "GITHUB_WORKSPACE", "/github/workspace".into());
        let name = format!("moloko-step-{}-{}", self.inst.row, self.next());
        let ws = self.ws.display().to_string();
        let temp = self.temp.display().to_string();
        let mut cmd = Command::new("docker");
        cmd.args(["run", "--rm", "--name", &name]);
        if !self.host {
            cmd.args(["--network", &format!("container:{}", self.container)]);
        }
        for v in [
            format!("{}:/github/workspace", self.volume.as_deref().unwrap_or(&ws)),
            format!("{}:{ws}", self.volume.as_deref().unwrap_or(&ws)),
            format!("{temp}:{temp}"),
            format!("{DOCKER_SOCK}:{DOCKER_SOCK}"),
        ] {
            cmd.arg("-v").arg(v);
        }
        cmd.args(["-w", "/github/workspace"]);
        for (k, v) in &env {
            cmd.env(k, v).arg("-e").arg(k);
        }
        if let Some(e) = entrypoint {
            cmd.arg("--entrypoint").arg(e);
        }
        cmd.arg(image).args(&args);
        let r = stream(cmd, &|l| self.handle_line(l), cancel).await?;
        if matches!(r, StepResult::Cancelled) {
            let _ = docker_out(&strs(&["rm", "-f", &name])).await;
        }
        let (outputs, _) = self.apply_files(&files);
        let outcome = match r {
            StepResult::Exited(0) => "success",
            StepResult::Exited(c) => {
                self.line(&format!("✗ container exited with code {c}"));
                "failure"
            }
            StepResult::Cancelled => "cancelled",
        };
        Ok((outcome, outputs))
    }

    // ---------- native actions ----------

    async fn native_checkout(&mut self, with: &[(String, String)], step_env: &[(String, String)], cancel: &CancellationToken) -> Result<Outcome> {
        let get = |k: &str| {
            with.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let engine = &self.inst.engine;
        let repo_id = self.inst.spec.repo.id;
        let rel = get("path").unwrap_or_default();
        if Path::new(&rel).components().any(|c| matches!(c, std::path::Component::ParentDir)) || rel.starts_with('/') {
            bail!("checkout path {rel:?} must stay inside the workspace");
        }
        let dest = self.ws.join(&rel);
        let requested = get("ref");
        let sha = match &requested {
            Some(r) => engine.git.resolve(&self.mirror, r).await?,
            None => self.inst.spec.sha.clone(),
        };
        let already = self.worktrees.iter().position(|w| w == &dest);
        if self.volume.is_some() {
            // The workspace lives in the container's volume, so git runs there (the mirror is mounted).
            let (d, m) = (dest.display().to_string(), self.mirror.display().to_string());
            let lock = engine.git.lock(repo_id);
            let _g = lock.lock().await;
            if let Some(pos) = already {
                let _ = self.job_out(&strs(&["git", "-c", "safe.directory=*", "-C", &m, "worktree", "remove", "--force", &d])).await;
                self.worktrees.remove(pos);
            }
            if self.job_out(&strs(&["sh", "-c", &format!("[ -z \"$(ls -A {} 2>/dev/null)\" ]", sh_quote(&d))])).await.is_err() {
                bail!("checkout target {} is not empty", if rel.is_empty() { "(workspace)" } else { &rel });
            }
            self.job_out(&strs(&["git", "-c", "safe.directory=*", "-C", &m, "worktree", "add", "--detach", "--force", &d, &sha]))
                .await
                .context("checkout")?;
        } else {
            if let Some(pos) = already {
                engine.git.remove(repo_id, &dest).await;
                self.worktrees.remove(pos);
            }
            if dest.exists() && std::fs::read_dir(&dest)?.next().is_some() {
                bail!("checkout target {} is not empty", if rel.is_empty() { "(workspace)" } else { &rel });
            }
            if let Some(p) = dest.parent() {
                tokio::fs::create_dir_all(p).await?;
            }
            engine.git.checkout(repo_id, &sha, &dest).await?;
        }
        self.worktrees.push(dest.clone());
        self.line(&format!(
            "  checked out {} into {}",
            &sha[..sha.len().min(10)],
            if rel.is_empty() { "the workspace" } else { &rel }
        ));
        if let Some(sub) = get("submodules").filter(|s| s == "true" || s == "recursive") {
            let mut argv = strs(&["git", "submodule", "update", "--init"]);
            if sub == "recursive" {
                argv.push("--recursive".into());
            }
            let scope = Scope {
                steps: Map::new(),
                inputs: Map::new(),
                env: self.job_env.clone(),
                action_path: None,
                failed: false,
                depth: 0,
                composite: false,
            };
            let (o, _) = self.exec_process(&argv, &scope, step_env, &dest, cancel).await?;
            if o != "success" {
                return Ok((o, Map::new()));
            }
        }
        if get("lfs").as_deref() == Some("true") {
            self.line("⚠ lfs: true isn't supported by moloko's built-in checkout");
        }
        let mut out = Map::new();
        out.insert("ref".into(), Value::String(requested.unwrap_or_else(|| self.inst.spec.git_ref.clone())));
        out.insert("commit".into(), Value::String(sha));
        Ok(("success", out))
    }

    fn container_path(&self, p: &str) -> String {
        let p = p.trim();
        if p == "~" {
            self.home.clone()
        } else if let Some(rest) = p.strip_prefix("~/") {
            format!("{}/{rest}", self.home)
        } else if p.starts_with('/') {
            p.to_string()
        } else {
            self.ws.join(p).display().to_string()
        }
    }

    fn cache_root(&self) -> PathBuf {
        self.inst.engine.gha.cache_dir.join(self.inst.spec.repo.id.to_string())
    }

    fn cache_entry(&self, key: &str) -> PathBuf {
        self.cache_root().join(hex::encode(Sha256::digest(key.as_bytes())))
    }

    /// Exact key first, then each restore key: exact, then newest prefix match.
    fn find_cache(&self, key: &str, restore_keys: &[String]) -> Option<(PathBuf, String, bool)> {
        let exact = self.cache_entry(key);
        if exact.join("key").exists() {
            return Some((exact, key.to_string(), true));
        }
        let mut entries: Vec<(PathBuf, String, std::time::SystemTime)> = std::fs::read_dir(self.cache_root())
            .ok()?
            .flatten()
            .filter_map(|e| {
                let k = std::fs::read_to_string(e.path().join("key")).ok()?;
                let t = e.path().join("key").metadata().ok()?.modified().ok()?;
                Some((e.path(), k, t))
            })
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.2));
        for rk in restore_keys {
            if let Some(e) = entries.iter().find(|e| &e.1 == rk).or_else(|| entries.iter().find(|e| e.1.starts_with(rk.as_str()))) {
                return Some((e.0.clone(), e.1.clone(), false));
            }
        }
        None
    }

    async fn native_cache(&mut self, which: &str, with: &[(String, String)]) -> Result<Outcome> {
        let get = |k: &str| with.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        let key = get("key").filter(|k| !k.trim().is_empty()).ok_or_else(|| anyhow!("cache: `key` is required"))?;
        let key = key.trim().to_string();
        let paths = lines(get("path").as_deref());
        if paths.is_empty() {
            bail!("cache: `path` is required");
        }
        if which == "actions/cache/save" {
            self.cache_save(&key, &paths).await?;
            return Ok(("success", Map::new()));
        }
        let restore_keys = lines(get("restore-keys").as_deref());
        let lookup_only = get("lookup-only").as_deref() == Some("true");
        let mut out = Map::new();
        out.insert("cache-primary-key".into(), Value::String(key.clone()));
        let found = self.find_cache(&key, &restore_keys);
        let exact = found.as_ref().is_some_and(|f| f.2);
        match &found {
            Some((entry, matched, _)) => {
                if !lookup_only {
                    for (i, p) in paths.iter().enumerate() {
                        let src = entry.join("data").join(i.to_string());
                        if !src.exists() {
                            continue;
                        }
                        let dst = self.container_path(p);
                        self.copy_in(&src, &dst).await?;
                    }
                }
                self.line(&format!("  cache restored from key: {matched}"));
                out.insert("cache-matched-key".into(), Value::String(matched.clone()));
            }
            None => {
                self.line(&format!("  cache not found for key: {key}"));
                out.insert("cache-matched-key".into(), Value::String(String::new()));
                if get("fail-on-cache-miss").as_deref() == Some("true") {
                    return Ok(("failure", out));
                }
            }
        }
        out.insert("cache-hit".into(), Value::String(exact.to_string()));
        if which == "actions/cache" && !exact {
            self.post.push(PostStep {
                name: "Post cache".into(),
                cond: "success()".into(),
                kind: PostKind::CacheSave { key, paths },
                env: Vec::new(),
            });
        }
        Ok(("success", out))
    }

    async fn cache_save(&self, key: &str, paths: &[String]) -> Result<()> {
        let entry = self.cache_entry(key);
        if entry.join("key").exists() {
            self.line(&format!("  cache entry for {key} already exists, not saving"));
            return Ok(());
        }
        let tmp = entry.with_extension(format!("tmp-{}", self.inst.row));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(tmp.join("data")).await?;
        let mut saved = 0;
        for (i, p) in paths.iter().enumerate() {
            if p.contains(['*', '?', '[']) || p.starts_with('!') {
                self.line(&format!("⚠ cache path {p:?}: patterns aren't supported, skipped"));
                continue;
            }
            let src = self.container_path(p);
            match self.copy_out(&src, &tmp.join("data").join(i.to_string())).await {
                Ok(_) => saved += 1,
                Err(_) => self.line(&format!("⚠ cache path {p} doesn't exist, skipped")),
            }
        }
        if saved == 0 {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            self.line("  nothing to cache");
            return Ok(());
        }
        tokio::fs::write(tmp.join("key"), key).await?;
        if tokio::fs::rename(&tmp, &entry).await.is_err() {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
        } else {
            self.line(&format!("  cache saved with key: {key}"));
        }
        Ok(())
    }

    fn artifacts_root(&self) -> PathBuf {
        self.inst.engine.gha.artifacts_dir.join(self.inst.spec.run_id.to_string())
    }

    /// Expand glob patterns inside the container (bash globstar if available).
    async fn expand(&self, patterns: &[String]) -> Result<Vec<String>> {
        if self.host {
            // macOS ships bash 3.2 without globstar, so match on the host in Rust.
            let mut files: Vec<String> = patterns.iter().flat_map(|p| expand_host(&self.container_path(p))).collect();
            files.sort();
            files.dedup();
            return Ok(files);
        }
        let script = "shopt -s globstar nullglob dotglob 2>/dev/null; IFS='\n'; for p in \"$@\"; do for f in $p; do [ -e \"$f\" ] && printf '%s\\n' \"$f\"; done; done; true";
        let mut out = String::new();
        for shell in ["bash", "sh"] {
            let mut args = strs(&[shell, "-c", script, "moloko"]);
            args.extend(patterns.iter().cloned());
            if let Ok(o) = self.job_out(&args).await {
                out = o;
                break;
            }
        }
        let mut files: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| self.container_path(l))
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    async fn native_upload(&mut self, with: &[(String, String)]) -> Result<Outcome> {
        let get = |k: &str| with.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
        let name = get("name").unwrap_or_else(|| "artifact".into());
        if name.contains(['/', '\\']) || name.starts_with('.') {
            bail!("invalid artifact name {name:?}");
        }
        let patterns: Vec<String> = lines(get("path").as_deref())
            .into_iter()
            .filter(|p| {
                if p.starts_with('!') {
                    self.line(&format!("⚠ exclude pattern {p} isn't supported, ignored"));
                }
                !p.starts_with('!')
            })
            .map(|p| if let Some(r) = p.strip_prefix("~/") { format!("{}/{r}", self.home) } else { p })
            .collect();
        let files = self.expand(&patterns).await?;
        if files.is_empty() {
            return match get("if-no-files-found").as_deref().unwrap_or("warn") {
                "error" => {
                    self.line(&format!("✗ no files found for artifact {name}"));
                    Ok(("failure", Map::new()))
                }
                "ignore" => Ok(("success", Map::new())),
                _ => {
                    self.line(&format!("⚠ no files found for artifact {name}, nothing uploaded"));
                    Ok(("success", Map::new()))
                }
            };
        }
        let dest = self.artifacts_root().join(&name);
        if dest.exists() {
            if get("overwrite").as_deref() == Some("true") {
                tokio::fs::remove_dir_all(&dest).await?;
            } else {
                bail!("artifact {name} already exists in this run (set overwrite: true to replace it)");
            }
        }
        tokio::fs::create_dir_all(&dest).await?;
        // Store paths relative to their least common ancestor, like upload-artifact.
        let single_dir = files.len() == 1
            && self.job_out(&strs(&["test", "-d", &files[0]])).await.is_ok();
        let lca: PathBuf = if single_dir {
            PathBuf::from(&files[0])
        } else {
            let mut it = files.iter().map(|f| Path::new(f).parent().unwrap_or(Path::new("/")).to_path_buf());
            let mut common = it.next().unwrap();
            for p in it {
                while !p.starts_with(&common) {
                    common = common.parent().unwrap_or(Path::new("/")).to_path_buf();
                }
            }
            common
        };
        for f in &files {
            let rel = Path::new(f).strip_prefix(&lca).unwrap_or(Path::new(""));
            let (src, target) = if rel.as_os_str().is_empty() {
                (format!("{f}/."), dest.clone())
            } else {
                let t = dest.join(rel);
                if let Some(p) = t.parent() {
                    tokio::fs::create_dir_all(p).await?;
                }
                (f.clone(), t)
            };
            self.copy_out(&src, &target).await?;
        }
        self.line(&format!("  uploaded artifact {name} ({} path{})", files.len(), if files.len() == 1 { "" } else { "s" }));
        let mut out = Map::new();
        out.insert("artifact-id".into(), Value::String(name));
        out.insert("artifact-url".into(), Value::String(String::new()));
        Ok(("success", out))
    }

    async fn native_download(&mut self, with: &[(String, String)]) -> Result<Outcome> {
        let get = |k: &str| with.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
        let dest = self.container_path(&get("path").unwrap_or_default());
        let root = self.artifacts_root();
        let mut copies: Vec<(PathBuf, String)> = Vec::new();
        if let Some(name) = get("name") {
            let src = root.join(&name);
            if !src.is_dir() {
                bail!("artifact {name} not found in this run");
            }
            copies.push((src, dest.clone()));
        } else {
            let pattern = get("pattern");
            let merge = get("merge-multiple").as_deref() == Some("true");
            let mut names: Vec<String> = std::fs::read_dir(&root)
                .map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
                .unwrap_or_default();
            names.sort();
            for n in names {
                if pattern.as_ref().is_none_or(|p| super::glob::matches(p, &n)) {
                    let target = if merge { dest.clone() } else { format!("{dest}/{n}") };
                    copies.push((root.join(&n), target));
                }
            }
        }
        for (src, target) in &copies {
            self.job_out(&strs(&["mkdir", "-p", target])).await?;
            self.copy_in(src, target).await?;
        }
        self.line(&format!("  downloaded {} artifact{} to {dest}", copies.len(), if copies.len() == 1 { "" } else { "s" }));
        let mut out = Map::new();
        out.insert("download-path".into(), Value::String(dest));
        Ok(("success", out))
    }
}

/// hashFiles() inside the job container: list the workspace's files there, select them with
/// the same patterns as on the host, and hash the selected ones there.
fn volume_hash_files(container: &str, ws: &str, patterns: &[String]) -> Result<String> {
    let run = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("docker").args(["exec", "-w", ws, container]).args(args).output()?;
        if !out.status.success() {
            bail!("hashFiles: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let listing = run(&["sh", "-c", "find . -type f -not -path '*/.git/*' | sed 's|^\\./||'"])?;
    let files: Vec<String> = listing.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    let selected = super::glob::select_files(&files, patterns);
    if selected.is_empty() {
        return Ok(String::new());
    }
    let mut args = vec!["sha256sum", "--"];
    args.extend(selected.iter().map(String::as_str));
    let sums = run(&args)?;
    let by_path: std::collections::HashMap<&str, &str> = sums.lines().filter_map(|l| l.split_once("  ").map(|(h, p)| (p, h))).collect();
    let digests = selected
        .iter()
        .map(|p| by_path.get(p.as_str()).and_then(|h| hex::decode(h).ok()).ok_or_else(|| anyhow!("hashFiles: no digest for {p}")))
        .collect::<Result<Vec<_>>>()?;
    Ok(super::glob::combine_digests(&digests))
}

/// Paths (files and directories) matching an absolute glob pattern (`*`, `**`, `?`, `[...]`).
fn expand_host(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim_end_matches('/');
    let first_glob = pattern.find(['*', '?', '[']);
    let Some(g) = first_glob else {
        return if Path::new(pattern).exists() { vec![pattern.trim_end_matches('/').to_string()] } else { vec![] };
    };
    let base = pattern[..g].rsplit_once('/').map(|(b, _)| if b.is_empty() { "/" } else { b }).unwrap_or(".");
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::from(base)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let s = path.display().to_string();
            if super::glob::matches(pattern, &s) {
                out.push(s);
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    out
}

/// The command line for a `run:` step's shell, with the script at `file`.
fn shell_argv(shell: Option<&str>, file: &str) -> Result<Vec<String>> {
    let f = file.to_string();
    Ok(match shell.map(str::trim) {
        // Unspecified: bash if the image has it, like GitHub; otherwise sh.
        None | Some("") => vec![
            "sh".into(),
            "-c".into(),
            "if command -v bash >/dev/null 2>&1; then exec bash -e \"$1\"; else exec sh -e \"$1\"; fi".into(),
            "sh".into(),
            f,
        ],
        Some("bash") => strs(&["bash", "--noprofile", "--norc", "-eo", "pipefail", &f]),
        Some("sh") => strs(&["sh", "-e", &f]),
        Some("python") => vec![
            "sh".into(),
            "-c".into(),
            "if command -v python >/dev/null 2>&1; then exec python \"$1\"; else exec python3 \"$1\"; fi".into(),
            "sh".into(),
            f,
        ],
        Some("pwsh" | "powershell") => vec!["pwsh".into(), "-command".into(), format!(". '{f}'")],
        Some(custom) if custom.contains("{0}") => split_words(custom).into_iter().map(|w| w.replace("{0}", &f)).collect(),
        Some(other) => bail!("shell {other:?} isn't supported (use bash, sh, python, pwsh or a command containing {{0}})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(split_words(r#"--health-cmd "pg_isready -U x" --health-interval 10s"#), ["--health-cmd", "pg_isready -U x", "--health-interval", "10s"]);
        assert_eq!(split_words("a 'b c' d\\ e \"\""), ["a", "b c", "d e", ""]);
        assert!(split_words("   ").is_empty());
    }

    #[test]
    fn uses_refs() {
        assert!(matches!(parse_uses("docker://alpine:3").unwrap(), Uses::Docker(i) if i == "alpine:3"));
        assert!(matches!(parse_uses("./.github/actions/x").unwrap(), Uses::Local(_)));
        match parse_uses("actions/cache/restore@v4").unwrap() {
            Uses::Remote { owner, repo, path, git_ref } => {
                assert_eq!((owner.as_str(), repo.as_str(), path.as_deref(), git_ref.as_str()), ("actions", "cache", Some("restore"), "v4"));
            }
            _ => panic!(),
        }
        assert!(parse_uses("actions/checkout").is_err());
        assert!(parse_uses("checkout@v4").is_err());
    }

    #[test]
    fn host_globs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        for f in ["target/release/app", "target/x/release/app.exe", "target/release/other", "README.md"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        let r = root.display();
        let mut got = expand_host(&format!("{r}/target/**/release/app*"));
        got.sort();
        assert_eq!(got, [format!("{r}/target/release/app"), format!("{r}/target/x/release/app.exe")]);
        assert_eq!(expand_host(&format!("{r}/README.md")), [format!("{r}/README.md")]);
        assert_eq!(expand_host(&format!("{r}/target/*/release/")), [format!("{r}/target/x/release")]);
        assert!(expand_host(&format!("{r}/nope*")).is_empty());
    }

    #[test]
    fn shells() {
        assert_eq!(shell_argv(Some("bash"), "/t/s.sh").unwrap(), ["bash", "--noprofile", "--norc", "-eo", "pipefail", "/t/s.sh"]);
        assert_eq!(shell_argv(Some("perl {0}"), "/t/s").unwrap(), ["perl", "/t/s"]);
        assert_eq!(shell_argv(None, "/t/s").unwrap()[0], "sh");
        assert!(shell_argv(Some("cmd"), "/t/s").is_err());
    }
}
