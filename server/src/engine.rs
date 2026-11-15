//! Turns a trigger into a run: sync the repo, read `.moloko.yml`, then execute the job graph
//! with as much parallelism as the executor slots allow.

use crate::agents::AgentHub;
use crate::cache::{self, Cache};
use crate::config::{JobInstance, JobSpec, PIPELINE_FILE, Pipeline};
use crate::db::{Db, Status};
use crate::executor::{Executor, StepResult};
use crate::gha::{self, trigger::Event, workflow::Workflow};
use crate::git::GitCache;
use crate::logs::LogHub;
use crate::registry::{self, Registry};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub struct Engine {
    pub db: Arc<Db>,
    pub logs: Arc<LogHub>,
    pub agents: AgentHub,
    pub registry: Registry,
    pub registry_host: String,
    pub gha: gha::Shared,
    pub vault: crate::secrets::Vault,
    pub(crate) git: GitCache,
    cache: Cache,
    pub(crate) work_dir: PathBuf,
    pub(crate) slots: Arc<Semaphore>,
    cancels: Mutex<HashMap<i64, CancellationToken>>,
}

/// What to run for one trigger.
struct Plan {
    sha: String,
    full_ref: String,
    before: Option<String>,
    event: &'static str,
    sources: Mutex<Vec<Source>>,
}

#[allow(clippy::large_enum_variant)] // one short-lived value per run
enum Source {
    Moloko(Result<Pipeline>),
    Workflow {
        file: String,
        /// The workflow and its `inputs` context.
        wf: Result<(Workflow, serde_json::Map<String, serde_json::Value>)>,
    },
}

/// Options for a manual run.
#[derive(Debug, Clone, Default)]
pub struct Dispatch {
    /// Run only this pipeline: `.moloko.yml` or a workflow file (path or file name).
    pub workflow: Option<String>,
    /// `workflow_dispatch` inputs.
    pub inputs: serde_json::Map<String, serde_json::Value>,
}

/// Everything a job needs that is shared across the run.
struct RunCtx {
    run_id: i64,
    repo_id: i64,
    /// `<project>/<repo>` in the built-in registry.
    image_path: String,
    git_ref: String,
    sha: String,
    pipeline: Pipeline,
    cancel: CancellationToken,
    /// Read-write registry credential, valid only while the run is active.
    registry_token: String,
    /// The repo's vault secrets; jobs get the ones they list under `secrets:`.
    secrets: Vec<(String, String)>,
}

/// Remove what jobs of a previous process left behind when it was stopped mid-run:
/// their containers, workspace volumes and workspace directories.
fn remove_leftovers(jobs: &[i64], work_dir: &std::path::Path) {
    let docker = |args: &[String]| {
        std::process::Command::new("docker")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    for id in jobs {
        let mut ps = vec!["ps".to_string(), "-aq".into()];
        for pattern in [format!("^moloko-gha-{id}$"), format!("^moloko-job-{id}$"), format!("^moloko-svc-{id}-")] {
            ps.extend(["--filter".into(), format!("name={pattern}")]);
        }
        let containers = docker(&ps);
        if !containers.trim().is_empty() {
            let mut rm = vec!["rm".to_string(), "-f".into()];
            rm.extend(containers.split_whitespace().map(String::from));
            docker(&rm);
        }
        docker(&["volume".into(), "rm".into(), "-f".into(), format!("moloko-ws-{id}")]);
        for dir in [format!("{id}"), format!("{id}-temp")] {
            let _ = std::fs::remove_dir_all(work_dir.join(dir));
        }
    }
}

impl Engine {
    /// `registry_host` is how jobs and agents address the built-in registry, e.g. `moloko.tail1234.ts.net`.
    pub fn new(
        data_dir: &std::path::Path,
        executors: usize,
        registry_host: &str,
        gha_config: gha::GhaConfig,
    ) -> Result<Arc<Self>> {
        for d in ["git", "cache", "logs", "work"] {
            std::fs::create_dir_all(data_dir.join(d))?;
        }
        let work_dir = data_dir.join("work").canonicalize()?;
        let db = Db::open(&data_dir.join("moloko.db"))?;
        let (interrupted, work) = (db.1.clone(), work_dir.clone());
        std::thread::spawn(move || remove_leftovers(&interrupted, &work));
        Ok(Arc::new(Engine {
            db: Arc::new(db),
            logs: Arc::new(LogHub::new(&data_dir.join("logs"))),
            agents: AgentHub::default(),
            registry: Registry::new(&data_dir.join("registry"))?,
            registry_host: registry_host.to_string(),
            gha: gha::Shared::new(data_dir, gha_config)?,
            vault: crate::secrets::Vault::open(
                data_dir,
                std::env::var("MOLOKO_SECRETS_KEY").ok().as_deref(),
            )?,
            git: GitCache::new(&data_dir.join("git")),
            cache: Cache::new(&data_dir.join("cache")),
            work_dir,
            slots: Arc::new(Semaphore::new(executors.max(1))),
            cancels: Mutex::default(),
        }))
    }

    /// Queue a run and return its id immediately; the work happens in the background.
    /// After syncing, every pipeline source at the commit (`.moloko.yml` and each matching
    /// GitHub workflow) becomes its own run; the first one reuses the returned id.
    pub fn trigger(
        self: &Arc<Self>,
        repo_id: i64,
        git_ref: &str,
        sha: Option<&str>,
        before: Option<&str>,
        trigger: &str,
        dispatch: Dispatch,
    ) -> Result<i64> {
        let repo = self
            .db
            .repo(repo_id)?
            .ok_or_else(|| anyhow!("repo {repo_id} not found"))?;
        let run_id = self.db.create_run(repo_id, git_ref, trigger)?;
        let cancel = CancellationToken::new();
        self.cancels.lock().unwrap().insert(run_id, cancel.clone());
        let this = self.clone();
        let (git_ref, sha, before, trigger) = (
            git_ref.to_string(),
            sha.map(str::to_string),
            before.map(str::to_string),
            trigger.to_string(),
        );
        tokio::spawn(async move {
            let plan = match this.plan(&repo, &git_ref, sha, before, &trigger, &dispatch).await {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    tracing::warn!(run_id, "run failed before start: {e:#}");
                    let _ = this
                        .db
                        .finish_run(run_id, Status::Failed, Some(&format!("{e:#}")));
                    this.cancels.lock().unwrap().remove(&run_id);
                    return;
                }
            };
            let repo = Arc::new(repo);
            let mut sources = std::mem::take(&mut *plan.sources.lock().unwrap());
            for (i, source) in sources.drain(..).enumerate() {
                let (rid, cancel) = if i == 0 {
                    (run_id, cancel.clone())
                } else {
                    let Ok(rid) = this.db.create_run(repo.id, &git_ref, &trigger) else {
                        continue;
                    };
                    let c = CancellationToken::new();
                    this.cancels.lock().unwrap().insert(rid, c.clone());
                    (rid, c)
                };
                let (this, repo, plan) = (this.clone(), repo.clone(), plan.clone());
                tokio::spawn(async move {
                    let status = this.run_source(rid, cancel, &repo, &plan, source).await;
                    this.cancels.lock().unwrap().remove(&rid);
                    tracing::info!(run_id = rid, status = status.as_str(), "run finished");
                });
            }
        });
        Ok(run_id)
    }

    /// Drop a repo's git mirror; the next run clones again (from the current URL).
    pub async fn forget_repo(&self, repo_id: i64) {
        self.git.forget(repo_id).await;
    }

    pub fn cancel(&self, run_id: i64) -> bool {
        match self.cancels.lock().unwrap().get(&run_id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// Decrypted secrets for a repo's runs: the project's, overridden by the repo's own.
    pub fn secrets_for(&self, repo: &crate::db::Repo) -> Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        for scope in [
            crate::secrets::Scope::Project(repo.project_id).key(),
            crate::secrets::Scope::Repo(repo.id).key(),
        ] {
            for (name, nonce, sealed) in self.db.sealed_secrets(&scope)? {
                let value = self.vault.open_value(&scope, &name, &nonce, &sealed)?;
                match out.iter_mut().find(|(n, _)| *n == name) {
                    Some(e) => e.1 = value,
                    None => out.push((name, value)),
                }
            }
        }
        Ok(out)
    }

    /// Pipelines that a manual run of `git_ref` can start, with their `workflow_dispatch` inputs.
    pub async fn manual_pipelines(&self, repo: &crate::db::Repo, git_ref: &str) -> Result<serde_json::Value> {
        let mirror = self.git.sync(repo.id, &repo.url).await.context("git sync")?;
        let sha = self.git.resolve(&mirror, git_ref).await?;
        let mut out = Vec::new();
        if self.git.read_file(&mirror, &sha, PIPELINE_FILE).await.is_ok() {
            out.push(serde_json::json!({ "file": PIPELINE_FILE, "name": PIPELINE_FILE, "inputs": {} }));
        }
        for name in self.git.list_dir(&mirror, &sha, gha::workflow::WORKFLOWS_DIR).await {
            let file = format!("{}/{name}", gha::workflow::WORKFLOWS_DIR);
            if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
                continue;
            }
            let Ok(wf) = self.git.read_file(&mirror, &sha, &file).await.and_then(|s| Workflow::parse(&s)) else {
                continue;
            };
            if let Some(cfg) = gha::trigger::event_config(&wf.on, "workflow_dispatch") {
                let inputs = cfg.get("inputs").cloned().unwrap_or_else(|| serde_json::json!({}));
                out.push(serde_json::json!({ "file": file, "name": wf.display_name(&file), "inputs": inputs }));
            }
        }
        Ok(serde_json::Value::Array(out))
    }

    /// Sync the repo and collect what to run at the commit.
    async fn plan(
        &self,
        repo: &crate::db::Repo,
        git_ref: &str,
        sha: Option<String>,
        before: Option<String>,
        trigger: &str,
        dispatch: &Dispatch,
    ) -> Result<Plan> {
        let mirror = self
            .git
            .sync(repo.id, &repo.url)
            .await
            .context("git sync")?;
        let sha = match sha {
            Some(s) => s,
            None => self.git.resolve(&mirror, git_ref).await?,
        };
        let full_ref = self.git.full_ref(&mirror, git_ref).await;
        let mut sources = Vec::new();
        let wanted = |file: &str| {
            dispatch.workflow.as_deref().is_none_or(|w| {
                w == file || file.rsplit('/').next() == Some(w)
            })
        };
        if wanted(PIPELINE_FILE)
            && let Ok(src) = self.git.read_file(&mirror, &sha, PIPELINE_FILE).await
        {
            sources.push(Source::Moloko(Pipeline::parse(&src)));
        }
        let push = trigger == "push";
        let changed = match (&before, push) {
            (Some(b), true) => self.git.changed_files(&mirror, b, &sha).await,
            _ => None,
        };
        let event = if push {
            Event::Push {
                git_ref: &full_ref,
                changed: changed.as_deref(),
            }
        } else {
            Event::Manual
        };
        let mut skipped = Vec::new();
        for name in self.git.list_dir(&mirror, &sha, gha::workflow::WORKFLOWS_DIR).await {
            if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
                continue;
            }
            let file = format!("{}/{name}", gha::workflow::WORKFLOWS_DIR);
            if !wanted(&file) {
                continue;
            }
            let parsed = self
                .git
                .read_file(&mirror, &sha, &file)
                .await
                .and_then(|src| Workflow::parse(&src));
            match parsed {
                Ok(wf) if gha::trigger::matches(&wf.on, &event) => {
                    let inputs = if push {
                        Ok(Default::default())
                    } else {
                        gha::trigger::resolve_inputs(&wf.on, &dispatch.inputs)
                    };
                    match inputs {
                        Ok(inputs) => sources.push(Source::Workflow { file, wf: Ok((wf, inputs)) }),
                        Err(e) => sources.push(Source::Workflow { file, wf: Err(e) }),
                    }
                }
                Ok(_) => skipped.push(name),
                // Invalid workflows still get a (failed) run, so the error is visible.
                Err(e) => sources.push(Source::Workflow { file, wf: Err(e) }),
            }
        }
        if sources.is_empty()
            && let Some(w) = &dispatch.workflow
        {
            anyhow::bail!("{w} doesn't exist at {sha} or doesn't listen for {}", event.name());
        }
        if sources.is_empty() {
            let mut msg = format!("nothing to run at {sha}: no {PIPELINE_FILE}");
            if skipped.is_empty() {
                msg.push_str(" and no workflows in .github/workflows");
            } else {
                msg.push_str(&format!(
                    ", and no workflow in .github/workflows listens for this {} ({})",
                    event.name(),
                    skipped.join(", ")
                ));
                if !push {
                    msg.push_str(". Add `workflow_dispatch:` under `on:` to run a workflow manually");
                }
            }
            anyhow::bail!(msg);
        }
        let event = event.name();
        Ok(Plan {
            sha,
            full_ref,
            before,
            event,
            sources: Mutex::new(sources),
        })
    }

    async fn run_source(
        self: &Arc<Self>,
        run_id: i64,
        cancel: CancellationToken,
        repo: &crate::db::Repo,
        plan: &Plan,
        source: Source,
    ) -> Status {
        let image_path = format!(
            "{}/{}",
            registry::repo_component(&repo.project_name),
            registry::repo_component(&repo.name)
        );
        let label = match &source {
            Source::Moloko(_) => PIPELINE_FILE.to_string(),
            Source::Workflow { file, wf } => wf
                .as_ref()
                .map(|(w, _)| w.display_name(file))
                .unwrap_or_else(|_| file.clone()),
        };
        let _ = self.db.set_run_workflow(run_id, &label);
        let _ = self.db.start_run(run_id, &plan.sha);
        let secrets = match self.secrets_for(repo) {
            Ok(s) => s,
            Err(e) => {
                let _ = self.db.finish_run(run_id, Status::Failed, Some(&format!("{e:#}")));
                return Status::Failed;
            }
        };
        let fail = |e: &anyhow::Error| {
            let _ = self
                .db
                .finish_run(run_id, Status::Failed, Some(&format!("{e:#}")));
            Status::Failed
        };
        match source {
            Source::Moloko(Err(e)) => fail(&e),
            Source::Workflow { file, wf: Err(e) } => fail(&e.context(file)),
            Source::Moloko(Ok(pipeline)) => {
                let ctx = RunCtx {
                    run_id,
                    repo_id: repo.id,
                    image_path,
                    git_ref: plan.full_ref.clone(),
                    sha: plan.sha.clone(),
                    pipeline,
                    cancel,
                    registry_token: crate::agents::new_token(),
                    secrets,
                };
                self.execute(Arc::new(ctx)).await
            }
            Source::Workflow { file, wf: Ok((workflow, inputs)) } => {
                let spec = Arc::new(gha::RunSpec {
                    run_id,
                    repo: repo.clone(),
                    git_ref: plan.full_ref.clone(),
                    sha: plan.sha.clone(),
                    before: plan.before.clone(),
                    event: plan.event,
                    inputs,
                    file,
                    workflow,
                    cancel,
                    registry_token: crate::agents::new_token(),
                    image: format!("{}/{image_path}", self.registry_host),
                    secrets,
                });
                self.registry.grant_run_token(&spec.registry_token);
                let (status, error) = match gha::run(self.clone(), spec.clone()).await {
                    Ok(s) => (s, None),
                    Err(e) => (Status::Failed, Some(format!("{e:#}"))),
                };
                self.registry.revoke_run_token(&spec.registry_token);
                let _ = self.db.finish_run(run_id, status, error.as_deref());
                status
            }
        }
    }

    async fn execute(self: &Arc<Self>, ctx: Arc<RunCtx>) -> Status {
        self.registry.grant_run_token(&ctx.registry_token);
        let (status, error) = match self.execute_jobs(&ctx).await {
            Ok(s) => (s, None),
            Err(e) => (Status::Failed, Some(format!("{e:#}"))),
        };
        self.registry.revoke_run_token(&ctx.registry_token);
        let _ = self.db.finish_run(ctx.run_id, status, error.as_deref());
        status
    }

    async fn execute_jobs(self: &Arc<Self>, ctx: &Arc<RunCtx>) -> Result<Status> {
        let mut pending: Vec<(i64, JobInstance)> = Vec::new();
        for inst in ctx.pipeline.expand() {
            let id = self.db.create_job(ctx.run_id, &inst.name, &inst.needs)?;
            pending.push((id, inst));
        }

        let mut done: HashMap<String, Status> = HashMap::new();
        let mut running = JoinSet::new();
        loop {
            // Skip jobs whose dependencies can no longer succeed; start jobs whose dependencies all passed.
            // Repeat until nothing changes: skipping one job can unblock the decision for another.
            let mut changed = true;
            while changed {
                changed = false;
                let mut i = 0;
                while i < pending.len() {
                    let (id, inst) = &pending[i];
                    let blocked = inst
                        .needs
                        .iter()
                        .any(|n| matches!(done.get(n), Some(s) if *s != Status::Success));
                    let ready = inst
                        .needs
                        .iter()
                        .all(|n| done.get(n) == Some(&Status::Success));
                    if blocked || (ctx.cancel.is_cancelled() && !ready) {
                        let st = if ctx.cancel.is_cancelled() {
                            Status::Cancelled
                        } else {
                            Status::Skipped
                        };
                        let _ = self.db.finish_job(*id, st, None);
                        done.insert(inst.name.clone(), st);
                        pending.swap_remove(i);
                        changed = true;
                    } else if ready {
                        let (id, inst) = pending.swap_remove(i);
                        let (this, ctx) = (self.clone(), ctx.clone());
                        running.spawn(async move {
                            let st = this.run_job(&ctx, id, &inst).await;
                            (inst.name, st)
                        });
                    } else {
                        i += 1;
                    }
                }
            }
            match running.join_next().await {
                Some(Ok((name, st))) => {
                    done.insert(name, st);
                }
                Some(Err(e)) => tracing::error!("job task panicked: {e}"),
                None => break,
            }
        }

        let status = if ctx.cancel.is_cancelled() {
            Status::Cancelled
        } else if done.values().all(|s| *s == Status::Success) {
            Status::Success
        } else {
            Status::Failed
        };
        Ok(status)
    }

    async fn run_job(&self, ctx: &RunCtx, job_id: i64, inst: &JobInstance) -> Status {
        let spec = &ctx.pipeline.jobs[&inst.base];
        // Agent jobs run elsewhere, so they don't take a local executor slot.
        let _permit = if spec.agent.is_empty() {
            tokio::select! {
                p = self.slots.clone().acquire_owned() => Some(p.expect("semaphore closed")),
                _ = ctx.cancel.cancelled() => {
                    let _ = self.db.finish_job(job_id, Status::Cancelled, None);
                    return Status::Cancelled;
                }
            }
        } else {
            None
        };
        let log = match self.logs.open(job_id) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(job_id, "cannot open log: {e}");
                let _ = self.db.finish_job(job_id, Status::Failed, None);
                return Status::Failed;
            }
        };
        log.mask(ctx.secrets.iter().map(|(_, v)| v.as_str()));
        // Open the log before marking the job running, so a viewer that sees `running` can stream it.
        let _ = self.db.start_job(job_id);
        if let Some(missing) = spec.secrets.iter().find(|n| !ctx.secrets.iter().any(|(k, _)| k.eq_ignore_ascii_case(n))) {
            log.line(&format!("✗ secret {missing} isn't set: add it to the repo or project in the dashboard"));
            drop(log);
            self.logs.close(job_id);
            let _ = self.db.finish_job(job_id, Status::Failed, None);
            return Status::Failed;
        }
        let ws = self.work_dir.join(job_id.to_string());
        let started = Instant::now();
        let env = job_env(ctx, inst, spec, &self.registry_host);

        let (status, code) = if !spec.agent.is_empty() {
            self.run_on_agents(ctx, spec, &env, &log).await
        } else {
            match self.git.checkout(ctx.repo_id, &ctx.sha, &ws).await {
                Ok(()) => {
                    let r = self
                        .run_steps(ctx, job_id, inst, spec, &env, &ws, &log)
                        .await;
                    self.git.remove(ctx.repo_id, &ws).await;
                    r
                }
                Err(e) => {
                    log.line(&format!("✗ checkout failed: {e:#}"));
                    (Status::Failed, None)
                }
            }
        };
        log.line(&format!(
            "● {} in {:.1}s",
            status.as_str(),
            started.elapsed().as_secs_f64()
        ));
        drop(log);
        self.logs.close(job_id);
        let _ = self.db.finish_job(job_id, status, code);
        status
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_steps(
        &self,
        ctx: &RunCtx,
        job_id: i64,
        inst: &JobInstance,
        spec: &JobSpec,
        env: &[(String, String)],
        ws: &std::path::Path,
        log: &crate::logs::JobLog,
    ) -> (Status, Option<i32>) {
        let cache_keys: Vec<String> = spec
            .cache
            .iter()
            .map(|c| cache::key(&inst.base, c, ws))
            .collect();
        for (c, key) in spec.cache.iter().zip(&cache_keys) {
            let hit = self.cache.restore(key, &ws.join(&c.path)).await;
            log.line(&format!(
                "▶ cache {}: {}",
                c.path,
                if hit { "hit" } else { "miss" }
            ));
        }

        let exec = match &spec.image {
            None => Executor::Host,
            Some(image) => {
                match Executor::docker(image, &format!("moloko-job-{job_id}"), ws, env, log).await {
                    Ok(e) => e,
                    Err(e) => {
                        log.line(&format!("✗ {e:#}"));
                        return (Status::Failed, None);
                    }
                }
            }
        };

        let mut result = (Status::Success, Some(0));
        for step in &spec.steps {
            log.line(&format!("▶ {}", step.label()));
            match exec
                .run_step(step.script(), ws, env, &|l| log.line(l), &ctx.cancel)
                .await
            {
                Ok(StepResult::Exited(0)) => {}
                Ok(StepResult::Exited(code)) => {
                    log.line(&format!("✗ step exited with code {code}"));
                    result = (Status::Failed, Some(code));
                    break;
                }
                Ok(StepResult::Cancelled) => {
                    log.line("✗ cancelled");
                    result = (Status::Cancelled, None);
                    break;
                }
                Err(e) => {
                    log.line(&format!("✗ {e:#}"));
                    result = (Status::Failed, None);
                    break;
                }
            }
        }
        exec.shutdown().await;

        if result.0 == Status::Success {
            for (c, key) in spec.cache.iter().zip(&cache_keys) {
                if self.cache.save(key, &ws.join(&c.path)).await {
                    log.line(&format!("▶ cache {}: saved", c.path));
                }
            }
        }
        result
    }

    /// Run the job's steps on every matching agent in parallel; success only if all succeed.
    async fn run_on_agents(
        &self,
        ctx: &RunCtx,
        spec: &JobSpec,
        env: &[(String, String)],
        log: &crate::logs::JobLog,
    ) -> (Status, Option<i32>) {
        let targets = self.agents.matching(&spec.agent);
        if targets.is_empty() {
            log.line(&format!(
                "✗ no connected agent has labels [{}]",
                spec.agent.join(", ")
            ));
            return (Status::Failed, None);
        }
        let names: Vec<&str> = targets.iter().map(|c| c.agent.name.as_str()).collect();
        log.line(&format!("▶ deploying to {}", names.join(", ")));
        let results = futures_util::future::join_all(targets.iter().map(|conn| async move {
            let name = &conn.agent.name;
            let prefixed = |l: &str| log.line(&format!("[{name}] {l}"));
            for step in &spec.steps {
                prefixed(&format!("▶ {}", step.label()));
                match self
                    .agents
                    .exec(conn, step.script(), env, &prefixed, &ctx.cancel)
                    .await
                {
                    Ok(StepResult::Exited(0)) => {}
                    Ok(StepResult::Exited(code)) => {
                        prefixed(&format!("✗ step exited with code {code}"));
                        return (Status::Failed, Some(code));
                    }
                    Ok(StepResult::Cancelled) => return (Status::Cancelled, None),
                    Err(e) => {
                        prefixed(&format!("✗ {e:#}"));
                        return (Status::Failed, None);
                    }
                }
            }
            (Status::Success, Some(0))
        }))
        .await;
        results
            .into_iter()
            .find(|(s, _)| *s != Status::Success)
            .unwrap_or((Status::Success, Some(0)))
    }
}

fn job_env(
    ctx: &RunCtx,
    inst: &JobInstance,
    spec: &JobSpec,
    registry_host: &str,
) -> Vec<(String, String)> {
    let (idx, total) = inst.shard.unwrap_or((0, 1));
    let mut env: Vec<(String, String)> = vec![
        ("CI".into(), "true".into()),
        ("MOLOKO".into(), "true".into()),
        ("MOLOKO_RUN_ID".into(), ctx.run_id.to_string()),
        ("MOLOKO_JOB".into(), inst.name.clone()),
        ("MOLOKO_REF".into(), ctx.git_ref.clone()),
        ("MOLOKO_SHA".into(), ctx.sha.clone()),
        ("MOLOKO_SHARD_INDEX".into(), idx.to_string()),
        ("MOLOKO_SHARD_TOTAL".into(), total.to_string()),
        ("MOLOKO_REGISTRY".into(), registry_host.to_string()),
        ("MOLOKO_REGISTRY_USER".into(), "moloko".into()),
        ("MOLOKO_REGISTRY_TOKEN".into(), ctx.registry_token.clone()),
        (
            "MOLOKO_IMAGE".into(),
            format!("{registry_host}/{}", ctx.image_path),
        ),
    ];
    env.extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    for name in &spec.secrets {
        if let Some((k, v)) = ctx.secrets.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            env.push((k.clone(), v.clone()));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::fixture_repo;
    use crate::logs::Subscription;
    use std::time::Duration;

    async fn wait_done(engine: &Engine, run_id: i64) -> String {
        for _ in 0..200 {
            let run = engine.db.run(run_id).unwrap().unwrap();
            if !matches!(run.status.as_str(), "queued" | "running") {
                return run.status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run {run_id} did not finish");
    }

    fn log_of(engine: &Engine, job_id: i64) -> String {
        match engine.logs.subscribe(job_id) {
            Subscription::Done(s) => s,
            Subscription::Live(..) => panic!("job still live"),
        }
    }

    #[tokio::test]
    async fn runs_dag_with_shards_cache_and_skips() {
        let dir = tempfile::tempdir().unwrap();
        let pipeline = r#"
jobs:
  build:
    cache: [{path: out, key_files: [lock]}]
    steps:
      - name: compile
        run: mkdir -p out && echo built > out/bin && echo "building $MOLOKO_SHA for $MOLOKO_IMAGE"
  test:
    needs: [build]
    shards: 2
    steps: ['echo "shard $MOLOKO_SHARD_INDEX of $MOLOKO_SHARD_TOTAL"']
  broken:
    needs: [build]
    steps: [exit 3]
  after-broken:
    needs: [broken]
    steps: [echo unreachable]
"#;
        let repo = fixture_repo(dir.path(), &[(".moloko.yml", pipeline), ("lock", "1")]).await;
        let engine = Engine::new(&dir.path().join("data"), 4, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine
            .db
            .create_repo(pid, "api", repo.to_str().unwrap())
            .unwrap();

        let run = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        assert_eq!(wait_done(&engine, run).await, "failed");
        let jobs = engine.db.jobs(run).unwrap();
        let by_name: HashMap<_, _> = jobs.iter().map(|j| (j.name.as_str(), j)).collect();
        assert_eq!(by_name["build"].status, "success");
        assert_eq!(by_name["test[1/2]"].status, "success");
        assert_eq!(by_name["test[2/2]"].status, "success");
        assert_eq!(by_name["broken"].status, "failed");
        assert_eq!(by_name["broken"].exit_code, Some(3));
        assert_eq!(by_name["after-broken"].status, "skipped");
        assert!(log_of(&engine, by_name["test[2/2]"].id).contains("shard 1 of 2"));
        let build_log = log_of(&engine, by_name["build"].id);
        assert!(build_log.contains("cache out: miss") && build_log.contains("cache out: saved"));
        assert!(build_log.contains("building "));
        assert!(build_log.contains("for localhost:0/demo/api"));

        // Second run hits the cache.
        let run2 = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        wait_done(&engine, run2).await;
        let build2 = engine
            .db
            .jobs(run2)
            .unwrap()
            .into_iter()
            .find(|j| j.name == "build")
            .unwrap();
        assert!(log_of(&engine, build2.id).contains("cache out: hit"));
        // Worktrees are cleaned up.
        assert_eq!(
            std::fs::read_dir(dir.path().join("data/work"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn cancel_kills_running_step() {
        let dir = tempfile::tempdir().unwrap();
        let pipeline = "jobs:\n  slow:\n    steps: [sleep 30]\n  later:\n    needs: [slow]\n    steps: [echo hi]\n";
        let repo = fixture_repo(dir.path(), &[(".moloko.yml", pipeline)]).await;
        let engine = Engine::new(&dir.path().join("data"), 2, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine
            .db
            .create_repo(pid, "api", repo.to_str().unwrap())
            .unwrap();
        let run = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        for _ in 0..100 {
            if engine
                .db
                .jobs(run)
                .unwrap()
                .iter()
                .any(|j| j.status == "running")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let t = Instant::now();
        assert!(engine.cancel(run));
        assert_eq!(wait_done(&engine, run).await, "cancelled");
        assert!(t.elapsed() < Duration::from_secs(5));
        let statuses: Vec<_> = engine
            .db
            .jobs(run)
            .unwrap()
            .into_iter()
            .map(|j| j.status)
            .collect();
        assert_eq!(statuses, vec!["cancelled", "cancelled"]);
    }

    #[tokio::test]
    async fn manual_trigger_runs_moloko_and_dispatch_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture_repo(
            dir.path(),
            &[
                (".moloko.yml", "jobs:\n  a:\n    steps: [echo hi]\n"),
                (
                    ".github/workflows/manual.yml",
                    "name: Manual\non: workflow_dispatch\njobs:\n  off:\n    if: false\n    runs-on: ubuntu-latest\n    steps: [{run: echo never}]\n",
                ),
                (
                    ".github/workflows/push.yml",
                    "on: push\njobs:\n  x:\n    runs-on: ubuntu-latest\n    steps: [{run: echo x}]\n",
                ),
            ],
        )
        .await;
        let engine = Engine::new(&dir.path().join("data"), 2, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine
            .db
            .create_repo(pid, "api", repo.to_str().unwrap())
            .unwrap();
        let first = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        assert_eq!(wait_done(&engine, first).await, "success");
        let mut runs = Vec::new();
        for _ in 0..100 {
            runs = engine.db.runs(None, None, 10).unwrap();
            if runs.len() == 2 && runs.iter().all(|r| r.status == "success") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut names: Vec<_> = runs.iter().map(|r| r.workflow.clone().unwrap()).collect();
        names.sort();
        assert_eq!(names, [".moloko.yml", "Manual"]);
        let manual = runs.iter().find(|r| r.workflow.as_deref() == Some("Manual")).unwrap();
        assert_eq!(manual.git_ref, "main");
        let jobs = engine.db.jobs(manual.id).unwrap();
        assert_eq!((jobs[0].name.as_str(), jobs[0].status.as_str()), ("off", "skipped"));
    }

    #[tokio::test]
    async fn manual_trigger_without_dispatch_workflow_explains() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture_repo(
            dir.path(),
            &[(".github/workflows/ci.yml", "on: push\njobs:\n  x:\n    runs-on: ubuntu-latest\n    steps: [{run: echo x}]\n")],
        )
        .await;
        let engine = Engine::new(&dir.path().join("data"), 1, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine.db.create_repo(pid, "api", repo.to_str().unwrap()).unwrap();
        let run = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        assert_eq!(wait_done(&engine, run).await, "failed");
        let err = engine.db.run(run).unwrap().unwrap().error.unwrap();
        assert!(err.contains("ci.yml") && err.contains("workflow_dispatch"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_inputs_and_workflow_selection() {
        let dir = tempfile::tempdir().unwrap();
        let wf = "name: Release\non:\n  workflow_dispatch:\n    inputs:\n      version: {required: true}\njobs:\n  off:\n    if: inputs.version == 'never'\n    runs-on: ubuntu-latest\n    steps: [{run: echo x}]\n";
        let repo = fixture_repo(
            dir.path(),
            &[(".moloko.yml", "jobs:\n  a:\n    steps: [echo hi]\n"), (".github/workflows/release.yml", wf)],
        )
        .await;
        let engine = Engine::new(&dir.path().join("data"), 1, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine.db.create_repo(pid, "app", repo.to_str().unwrap()).unwrap();
        let only = |inputs: serde_json::Value| Dispatch {
            workflow: Some("release.yml".into()),
            inputs: inputs.as_object().unwrap().clone(),
        };
        let run = engine.trigger(rid, "main", None, None, "manual", only(serde_json::json!({}))).unwrap();
        assert_eq!(wait_done(&engine, run).await, "failed");
        let err = engine.db.run(run).unwrap().unwrap().error.unwrap();
        assert!(err.contains("`version` is required"), "{err}");
        let run = engine
            .trigger(rid, "main", None, None, "manual", only(serde_json::json!({"version": "0.3.0"})))
            .unwrap();
        assert_eq!(wait_done(&engine, run).await, "success");
        // Only the selected workflow ran: no .moloko.yml run was created.
        assert_eq!(engine.db.runs(None, None, 10).unwrap().len(), 2);
        let pipelines = engine.manual_pipelines(&engine.db.repo(rid).unwrap().unwrap(), "main").await.unwrap();
        assert_eq!(pipelines[1]["inputs"]["version"]["required"], true);
    }

    #[tokio::test]
    async fn vault_secrets_reach_steps_masked() {
        use crate::secrets::Scope;
        let dir = tempfile::tempdir().unwrap();
        let pipeline = "jobs:\n  use:\n    secrets: [token]\n    steps: ['test \"$TOKEN\" = repo-s3cret && echo \"got $TOKEN\" && echo \"$TOKEN\" | rev']\n  missing:\n    secrets: [NOPE]\n    steps: [echo never]\n";
        let repo = fixture_repo(dir.path(), &[(".moloko.yml", pipeline)]).await;
        let engine = Engine::new(&dir.path().join("data"), 2, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine.db.create_repo(pid, "api", repo.to_str().unwrap()).unwrap();
        for (scope, name, value) in [
            (Scope::Project(pid), "TOKEN", "project-value"),
            (Scope::Project(pid), "SHARED", "from-project"),
            (Scope::Repo(rid), "TOKEN", "repo-s3cret"),
        ] {
            let (n, c) = engine.vault.seal(scope, name, value).unwrap();
            engine.db.put_secret(&scope.key(), name, &n, &c).unwrap();
        }
        let r = engine.db.repo(rid).unwrap().unwrap();
        let mut secrets = engine.secrets_for(&r).unwrap();
        secrets.sort();
        assert_eq!(secrets, [("SHARED".into(), "from-project".into()), ("TOKEN".into(), "repo-s3cret".into())]);

        let run = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        assert_eq!(wait_done(&engine, run).await, "failed");
        let jobs = engine.db.jobs(run).unwrap();
        let by: HashMap<_, _> = jobs.iter().map(|j| (j.name.as_str(), j)).collect();
        assert_eq!(by["use"].status, "success");
        let log = log_of(&engine, by["use"].id);
        assert!(log.contains("got ***") && !log.contains("repo-s3cret"), "{log}");
        assert_eq!(by["missing"].status, "failed");
        assert!(log_of(&engine, by["missing"].id).contains("secret NOPE isn't set"));

        assert!(engine.db.delete_repo(rid).unwrap());
        assert!(engine.db.secret_names(&Scope::Repo(rid).key()).unwrap().is_empty());
        assert_eq!(engine.db.secret_names(&Scope::Project(pid).key()).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn missing_pipeline_fails_run_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture_repo(dir.path(), &[("README", "x")]).await;
        let engine = Engine::new(&dir.path().join("data"), 1, "localhost:0", Default::default()).unwrap();
        let pid = engine.db.create_project("demo").unwrap();
        let rid = engine
            .db
            .create_repo(pid, "api", repo.to_str().unwrap())
            .unwrap();
        let run = engine.trigger(rid, "main", None, None, "manual", Default::default()).unwrap();
        assert_eq!(wait_done(&engine, run).await, "failed");
        assert!(
            engine
                .db
                .run(run)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains(".moloko.yml")
        );
    }
}
