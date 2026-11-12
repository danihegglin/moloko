//! Turns a trigger into a run: sync the repo, read `.moloko.yml`, then execute the job graph
//! with as much parallelism as the executor slots allow.

use crate::agents::AgentHub;
use crate::cache::{self, Cache};
use crate::config::{JobInstance, JobSpec, PIPELINE_FILE, Pipeline};
use crate::db::{Db, Status};
use crate::executor::{Executor, StepResult};
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
    git: GitCache,
    cache: Cache,
    work_dir: PathBuf,
    slots: Arc<Semaphore>,
    cancels: Mutex<HashMap<i64, CancellationToken>>,
}

/// Everything a job needs that is shared across the run.
struct RunCtx {
    run_id: i64,
    project_id: i64,
    project_name: String,
    git_ref: String,
    sha: String,
    pipeline: Pipeline,
    cancel: CancellationToken,
    /// Read-write registry credential, valid only while the run is active.
    registry_token: String,
}

impl Engine {
    /// `registry_host` is how jobs and agents address the built-in registry, e.g. `moloko.tail1234.ts.net`.
    pub fn new(
        data_dir: &std::path::Path,
        executors: usize,
        registry_host: &str,
    ) -> Result<Arc<Self>> {
        for d in ["git", "cache", "logs", "work"] {
            std::fs::create_dir_all(data_dir.join(d))?;
        }
        let work_dir = data_dir.join("work").canonicalize()?;
        Ok(Arc::new(Engine {
            db: Arc::new(Db::open(&data_dir.join("moloko.db"))?),
            logs: Arc::new(LogHub::new(&data_dir.join("logs"))),
            agents: AgentHub::default(),
            registry: Registry::new(&data_dir.join("registry"))?,
            registry_host: registry_host.to_string(),
            git: GitCache::new(&data_dir.join("git")),
            cache: Cache::new(&data_dir.join("cache")),
            work_dir,
            slots: Arc::new(Semaphore::new(executors.max(1))),
            cancels: Mutex::default(),
        }))
    }

    /// Queue a run and return its id immediately; the work happens in the background.
    pub fn trigger(
        self: &Arc<Self>,
        project_id: i64,
        git_ref: &str,
        sha: Option<&str>,
        trigger: &str,
    ) -> Result<i64> {
        let project = self
            .db
            .project(project_id)?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let run_id = self.db.create_run(project_id, git_ref, trigger)?;
        let cancel = CancellationToken::new();
        self.cancels.lock().unwrap().insert(run_id, cancel.clone());
        let this = self.clone();
        let (git_ref, sha) = (git_ref.to_string(), sha.map(str::to_string));
        tokio::spawn(async move {
            let status = match this.prepare(run_id, &project, git_ref, sha, cancel).await {
                Ok(ctx) => this.execute(Arc::new(ctx)).await,
                Err(e) => {
                    tracing::warn!(run_id, "run failed before start: {e:#}");
                    let _ = this
                        .db
                        .finish_run(run_id, Status::Failed, Some(&format!("{e:#}")));
                    Status::Failed
                }
            };
            this.cancels.lock().unwrap().remove(&run_id);
            tracing::info!(run_id, status = status.as_str(), "run finished");
        });
        Ok(run_id)
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

    async fn prepare(
        &self,
        run_id: i64,
        project: &crate::db::Project,
        git_ref: String,
        sha: Option<String>,
        cancel: CancellationToken,
    ) -> Result<RunCtx> {
        let project_id = project.id;
        let mirror = self
            .git
            .sync(project_id, &project.repo_url)
            .await
            .context("git sync")?;
        let sha = match sha {
            Some(s) => s,
            None => self.git.resolve(&mirror, &git_ref).await?,
        };
        let src = self
            .git
            .read_file(&mirror, &sha, PIPELINE_FILE)
            .await
            .with_context(|| format!("no {PIPELINE_FILE} at {sha}"))?;
        let pipeline = Pipeline::parse(&src)?;
        self.db.start_run(run_id, &sha)?;
        Ok(RunCtx {
            run_id,
            project_id,
            project_name: project.name.clone(),
            git_ref,
            sha,
            pipeline,
            cancel,
            registry_token: crate::agents::new_token(),
        })
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
        // Open the log before marking the job running, so a viewer that sees `running` can stream it.
        let _ = self.db.start_job(job_id);
        let ws = self.work_dir.join(job_id.to_string());
        let started = Instant::now();
        let env = job_env(ctx, inst, spec, &self.registry_host);

        let (status, code) = if !spec.agent.is_empty() {
            self.run_on_agents(ctx, spec, &env, &log).await
        } else {
            match self.git.checkout(ctx.project_id, &ctx.sha, &ws).await {
                Ok(()) => {
                    let r = self
                        .run_steps(ctx, job_id, inst, spec, &env, &ws, &log)
                        .await;
                    self.git.remove(ctx.project_id, &ws).await;
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
            format!(
                "{registry_host}/{}",
                registry::repo_component(&ctx.project_name)
            ),
        ),
    ];
    env.extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
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
        run: mkdir -p out && echo built > out/bin && echo "building $MOLOKO_SHA"
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
        let engine = Engine::new(&dir.path().join("data"), 4, "localhost:0").unwrap();
        let pid = engine
            .db
            .create_project("demo", repo.to_str().unwrap())
            .unwrap();

        let run = engine.trigger(pid, "main", None, "manual").unwrap();
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

        // Second run hits the cache.
        let run2 = engine.trigger(pid, "main", None, "manual").unwrap();
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
        let engine = Engine::new(&dir.path().join("data"), 2, "localhost:0").unwrap();
        let pid = engine
            .db
            .create_project("demo", repo.to_str().unwrap())
            .unwrap();
        let run = engine.trigger(pid, "main", None, "manual").unwrap();
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
    async fn missing_pipeline_fails_run_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture_repo(dir.path(), &[("README", "x")]).await;
        let engine = Engine::new(&dir.path().join("data"), 1, "localhost:0").unwrap();
        let pid = engine
            .db
            .create_project("demo", repo.to_str().unwrap())
            .unwrap();
        let run = engine.trigger(pid, "main", None, "manual").unwrap();
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
