//! GitHub Actions compatibility: run `.github/workflows/*.yml` the way GitHub does.
//!
//! Each matching workflow becomes its own run. Jobs run in a runner-image container
//! (`runs-on` → image), steps run with `docker exec`, and `uses:` actions are fetched at
//! their ref and executed as node, docker or composite actions. `actions/checkout`,
//! `actions/cache` and `actions/{upload,download}-artifact` are implemented natively,
//! because the real ones need GitHub's hosted services.

pub mod commands;
pub mod expr;
pub mod glob;
mod job;
pub mod matrix;
pub mod setup;
pub mod trigger;
pub mod workflow;

use crate::db::{Repo, Status};
use crate::engine::Engine;
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use workflow::{Job, Workflow};

/// Server-wide settings for workflow runs.
#[derive(Debug, Clone, Default)]
pub struct GhaConfig {
    /// `runs-on` label → runner image overrides, e.g. `ubuntu-latest=ghcr.io/catthehacker/ubuntu:full-latest`.
    /// The image `host` runs the job directly on the build machine.
    pub images: Vec<(String, String)>,
    /// Used as `secrets.GITHUB_TOKEN` / `github.token`, and to fetch actions.
    pub github_token: Option<String>,
}

/// `runs-on` label → runner image. The `moloko/ubuntu:*` images are catthehacker's act images
/// (what nektos/act uses) plus the few packages GitHub's runner images have and they lack;
/// moloko builds them on first use.
const DEFAULT_IMAGES: &[(&str, &str)] = &[
    ("ubuntu-latest", "moloko/ubuntu:latest-1"),
    ("ubuntu-24.04", "moloko/ubuntu:24.04-1"),
    ("ubuntu-22.04", "moloko/ubuntu:22.04-1"),
    ("ubuntu-20.04", "moloko/ubuntu:20.04-1"),
    ("ubuntu-24.04-arm", "moloko/ubuntu:24.04-1"),
    ("ubuntu-22.04-arm", "moloko/ubuntu:22.04-1"),
];

/// Runner for `windows-*` jobs: Ubuntu with a Windows cross toolchain (see the Dockerfile).
pub const WINDOWS_CROSS_IMAGE: &str = "moloko/windows-cross:2";

/// Packages GitHub's Ubuntu runners include that the act images don't
/// (`xdg-utils`: Tauri's AppImage bundler needs `xdg-open`).
const UBUNTU_EXTRAS: &str = "xdg-utils";

/// Images moloko builds itself from a Dockerfile instead of pulling.
pub fn builtin_dockerfile(image: &str) -> Option<String> {
    if image == WINDOWS_CROSS_IMAGE {
        return Some(include_str!("../../runners/windows-cross.Dockerfile").to_string());
    }
    let tag = image.strip_prefix("moloko/ubuntu:")?.strip_suffix("-1")?;
    Some(format!(
        "FROM catthehacker/ubuntu:act-{tag}\n\
         RUN apt-get update && apt-get install -y --no-install-recommends {UBUNTU_EXTRAS} && rm -rf /var/lib/apt/lists/*\n"
    ))
}

impl GhaConfig {
    /// Parse `label=image,label=image`.
    pub fn parse_images(s: &str) -> Result<Vec<(String, String)>> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| {
                p.split_once('=')
                    .map(|(l, i)| (l.trim().to_string(), i.trim().to_string()))
                    .ok_or_else(|| anyhow!("expected label=image, got {p:?}"))
            })
            .collect()
    }

    /// Pick the image for a job's `runs-on` labels. Returns (image, note for the log).
    pub fn image_for(&self, labels: &[String]) -> Result<(String, Option<String>)> {
        for l in labels {
            if let Some((_, img)) = self.images.iter().find(|(k, _)| k.eq_ignore_ascii_case(l)) {
                return Ok((img.clone(), None));
            }
            if let Some((_, img)) = DEFAULT_IMAGES.iter().find(|(k, _)| k.eq_ignore_ascii_case(l)) {
                return Ok((img.to_string(), None));
            }
        }
        if let Some(l) = labels.iter().find(|l| l.to_lowercase().starts_with("macos")) {
            // macOS can't run in a container, but on a Mac the job can run on the host itself.
            if cfg!(target_os = "macos") {
                return Ok(("host".into(), None));
            }
            anyhow::bail!(
                "runs-on: {l} needs a Mac: macOS can't run in a container. Run moloko on a Mac to build macOS jobs on the host"
            );
        }
        if let Some(l) = labels.iter().find(|l| l.to_lowercase().starts_with("windows")) {
            // Windows can't run in a container here; cross-compile on Linux instead.
            return Ok((
                WINDOWS_CROSS_IMAGE.into(),
                Some(format!(
                    "runs-on: {l} runs on Linux with a Windows cross toolchain (cargo-xwin, clang-cl, NSIS), so runner.os is Linux. Build Rust with `--target x86_64-pc-windows-msvc` via cargo-xwin"
                )),
            ));
        }
        let default = self.image_for(&["ubuntu-latest".to_string()])?.0;
        Ok((
            default.clone(),
            Some(format!(
                "runs-on [{}] has no image mapping; using {default}",
                labels.join(", ")
            )),
        ))
    }
}

/// State shared by all workflow runs.
pub struct Shared {
    pub config: GhaConfig,
    pub actions_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub downloads: setup::Progress,
}

impl Shared {
    pub fn new(data_dir: &Path, config: GhaConfig) -> Result<Self> {
        let root = data_dir.join("gha");
        let s = Shared {
            config,
            actions_dir: root.join("actions"),
            cache_dir: root.join("cache"),
            artifacts_dir: root.join("artifacts"),
            locks: Mutex::default(),
            downloads: Mutex::default(),
        };
        for d in [&s.actions_dir, &s.cache_dir, &s.artifacts_dir] {
            std::fs::create_dir_all(d)?;
        }
        Ok(Shared {
            actions_dir: s.actions_dir.canonicalize()?,
            cache_dir: s.cache_dir.canonicalize()?,
            artifacts_dir: s.artifacts_dir.canonicalize()?,
            ..s
        })
    }

    /// cargo-xwin's MSVC CRT + Windows SDK download, kept between jobs.
    pub fn xwin_dir(&self) -> PathBuf {
        self.actions_dir.with_file_name("xwin-cache")
    }

    fn lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(key.to_string()).or_default().clone()
    }
}

/// One workflow run.
pub struct RunSpec {
    pub run_id: i64,
    pub repo: Repo,
    /// Full ref, e.g. `refs/heads/main`; empty when the run targets a bare commit.
    pub git_ref: String,
    pub sha: String,
    pub before: Option<String>,
    /// `push` or `workflow_dispatch`.
    pub event: &'static str,
    pub inputs: Map<String, Value>,
    /// Path of the workflow file in the repo.
    pub file: String,
    pub workflow: Workflow,
    pub cancel: CancellationToken,
    pub registry_token: String,
    /// `<registry>/<project>/<repo>`.
    pub image: String,
    /// Vault secrets (project, overridden by repo) for the `secrets` context.
    pub secrets: Vec<(String, String)>,
}

impl RunSpec {
    /// `owner/name` for the `github` context: from a GitHub-style URL, else project/repo.
    pub fn slug(&self) -> (String, String) {
        let norm = crate::api::normalize_repo_url(&self.repo.url);
        let parts: Vec<&str> = norm.split('/').collect();
        if parts.len() >= 3 && parts[0].contains('.') {
            (parts[parts.len() - 2].to_string(), parts[parts.len() - 1].to_string())
        } else {
            (self.repo.project_name.clone(), self.repo.name.clone())
        }
    }
}

#[derive(Debug, Clone)]
struct JobResult {
    /// success, failure, cancelled or skipped.
    result: &'static str,
    outputs: Map<String, Value>,
}

/// Run every job of the workflow; returns the run's final status.
pub async fn run(engine: Arc<Engine>, spec: Arc<RunSpec>) -> Result<Status> {
    let wf = &spec.workflow;
    // Run ids can be reused after deletions; start with a clean artifact store.
    let _ = tokio::fs::remove_dir_all(engine.gha.artifacts_dir.join(spec.run_id.to_string())).await;

    let mut jobs: Vec<(String, Job)> = Vec::new();
    let mut rows: HashMap<String, i64> = HashMap::new();
    for id in wf.jobs.keys() {
        let job = wf.job(id)?;
        rows.insert(id.clone(), engine.db.create_job(spec.run_id, id, &job.needs)?);
        jobs.push((id.clone(), job));
    }

    let mut results: HashMap<String, JobResult> = HashMap::new();
    let mut pending: Vec<(String, Job)> = jobs;
    let mut running: JoinSet<(String, JobResult)> = JoinSet::new();
    loop {
        let mut changed = true;
        while changed {
            changed = false;
            let mut i = 0;
            while i < pending.len() {
                if !pending[i].1.needs.iter().all(|n| results.contains_key(n)) {
                    i += 1;
                    continue;
                }
                let (id, job) = pending.swap_remove(i);
                changed = true;
                let row = rows[&id];
                let needs = needs_ctx(&job, &results);
                match decide(&spec, &job, &needs, &results) {
                    Ok(None) => {
                        let (engine, spec) = (engine.clone(), spec.clone());
                        running.spawn(async move {
                            let r = run_job_group(&engine, &spec, &id, &job, row, needs).await;
                            (id, r)
                        });
                    }
                    Ok(Some(result)) => {
                        let st = if result == "cancelled" { Status::Cancelled } else { Status::Skipped };
                        let _ = engine.db.finish_job(row, st, None);
                        results.insert(id, JobResult { result, outputs: Map::new() });
                    }
                    Err(e) => {
                        let log = engine.logs.open(row)?;
                        log.line(&format!("✗ {e:#}"));
                        drop(log);
                        engine.logs.close(row);
                        let _ = engine.db.finish_job(row, Status::Failed, None);
                        results.insert(id, JobResult { result: "failure", outputs: Map::new() });
                    }
                }
            }
        }
        match running.join_next().await {
            Some(Ok((id, r))) => {
                results.insert(id, r);
            }
            Some(Err(e)) => tracing::error!("workflow job task panicked: {e}"),
            None => break,
        }
    }

    Ok(if spec.cancel.is_cancelled() {
        Status::Cancelled
    } else if results.values().any(|r| r.result == "failure") {
        Status::Failed
    } else {
        Status::Success
    })
}

fn needs_ctx(job: &Job, results: &HashMap<String, JobResult>) -> Value {
    let mut m = Map::new();
    for n in &job.needs {
        if let Some(r) = results.get(n) {
            m.insert(n.clone(), json!({ "result": r.result, "outputs": r.outputs }));
        }
    }
    Value::Object(m)
}

/// Contexts available to job-level expressions (`if`, `strategy`, `runs-on`, ...).
pub(super) fn job_level_contexts(spec: &RunSpec, needs: &Value, extra: &[(&str, Value)]) -> Map<String, Value> {
    let (owner, name) = spec.slug();
    let mut m = Map::new();
    m.insert(
        "github".into(),
        json!({
            "event_name": spec.event,
            "ref": spec.git_ref,
            "ref_name": short_ref(&spec.git_ref),
            "ref_type": ref_type(&spec.git_ref),
            "sha": spec.sha,
            "repository": format!("{owner}/{name}"),
            "repository_owner": owner,
            "run_id": spec.run_id.to_string(),
            "run_number": spec.run_id.to_string(),
            "run_attempt": "1",
            "workflow": spec.workflow.display_name(&spec.file),
            "server_url": "https://github.com",
            "api_url": "https://api.github.com",
            "actor": "moloko",
            "triggering_actor": "moloko",
            "event": event_payload(spec),
        }),
    );
    m.insert("needs".into(), needs.clone());
    m.insert("inputs".into(), Value::Object(spec.inputs.clone()));
    m.insert("vars".into(), json!({}));
    m.insert("env".into(), Value::Object(spec.workflow.env.clone()));
    for (k, v) in extra {
        m.insert(k.to_string(), v.clone());
    }
    m
}

pub fn short_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("refs/tags/"))
        .unwrap_or(r)
}

fn ref_type(r: &str) -> &'static str {
    if r.starts_with("refs/tags/") { "tag" } else { "branch" }
}

fn event_payload(spec: &RunSpec) -> Value {
    let (owner, name) = spec.slug();
    let repository = json!({
        "name": name,
        "full_name": format!("{owner}/{name}"),
        "owner": { "login": owner, "name": owner },
        "clone_url": spec.repo.url,
    });
    if spec.event == "workflow_dispatch" {
        json!({ "ref": spec.git_ref, "inputs": spec.inputs, "repository": repository, "sender": { "login": "moloko" } })
    } else {
        json!({
            "ref": spec.git_ref,
            "before": spec.before.clone().unwrap_or_else(|| "0".repeat(40)),
            "after": spec.sha,
            "repository": repository,
            "head_commit": { "id": spec.sha },
            "sender": { "login": "moloko" },
        })
    }
}

/// Should the job run? `Ok(None)` = run it, `Ok(Some(result))` = skip/cancel with that result.
fn decide(
    spec: &RunSpec,
    job: &Job,
    needs: &Value,
    results: &HashMap<String, JobResult>,
) -> Result<Option<&'static str>> {
    let cond = cond_str(job.cond.as_ref());
    let uses_status = has_status_fn(&cond);
    let any = |r: &str| job.needs.iter().any(|n| results[n].result == r);
    if !uses_status && spec.cancel.is_cancelled() {
        return Ok(Some("cancelled"));
    }
    if !uses_status && (any("skipped") || any("cancelled")) {
        return Ok(Some("skipped"));
    }
    let contexts = job_level_contexts(spec, needs, &[]);
    let ctx = expr::Ctx {
        contexts: &contexts,
        status: expr::StatusCtx {
            failed: any("failure"),
            cancelled: spec.cancel.is_cancelled(),
        },
        workspace: None,
        hasher: None,
    };
    if expr::eval_if(&cond, &ctx)? {
        if job.uses.is_some() {
            anyhow::bail!("reusable workflows (jobs.<id>.uses) aren't supported yet");
        }
        Ok(None)
    } else {
        Ok(Some("skipped"))
    }
}

/// `if:` values can be YAML booleans or strings.
fn cond_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn has_status_fn(cond: &str) -> bool {
    let c = cond.to_lowercase();
    ["success(", "failure(", "always(", "cancelled("]
        .iter()
        .any(|f| c.contains(f))
}

/// Expand the matrix and run every combination; aggregate into one result for `needs`.
async fn run_job_group(
    engine: &Arc<Engine>,
    spec: &Arc<RunSpec>,
    id: &str,
    job: &Job,
    row: i64,
    needs: Value,
) -> JobResult {
    let fail = |msg: String| {
        if let Ok(log) = engine.logs.open(row) {
            log.line(&format!("✗ {msg}"));
        }
        engine.logs.close(row);
        let _ = engine.db.finish_job(row, Status::Failed, None);
        JobResult { result: "failure", outputs: Map::new() }
    };
    let contexts = job_level_contexts(spec, &needs, &[]);
    let ctx = expr::Ctx { contexts: &contexts, status: Default::default(), workspace: None, hasher: None };
    let strategy = job.strategy.as_ref();
    let combos = match strategy
        .and_then(|s| s.matrix.as_ref())
        .map(|m| expr::interpolate_value(m, &ctx))
        .transpose()
        .and_then(|m| matrix::expand(m.as_ref()))
    {
        Ok(c) => c,
        Err(e) => return fail(format!("strategy.matrix: {e:#}")),
    };
    let eval_opt = |v: Option<&Value>| -> Option<Value> {
        v.map(|v| expr::interpolate_value(v, &ctx).unwrap_or(Value::Null))
    };
    let fail_fast = eval_opt(strategy.and_then(|s| s.fail_fast.as_ref()))
        .map(|v| expr::truthy(&v))
        .unwrap_or(true);
    let max_parallel = eval_opt(strategy.and_then(|s| s.max_parallel.as_ref()))
        .and_then(|v| v.as_u64().or_else(|| expr::to_string(&v).parse().ok()))
        .filter(|n| *n > 0)
        .unwrap_or(combos.len() as u64) as usize;
    let continue_on_error = eval_opt(job.continue_on_error.as_ref())
        .map(|v| expr::truthy(&v))
        .unwrap_or(false);

    // The placeholder row becomes the first instance; the rest get their own rows.
    let mut instances = Vec::new();
    for (i, combo) in combos.iter().enumerate() {
        let name = matrix::instance_name(id, combo);
        let r = if i == 0 {
            if name != id {
                let _ = engine.db.rename_job(row, &name);
            }
            row
        } else {
            match engine.db.create_job(spec.run_id, &name, &job.needs) {
                Ok(r) => r,
                Err(e) => return fail(format!("{e:#}")),
            }
        };
        instances.push((r, combo.clone()));
    }

    let group_cancel = spec.cancel.child_token();
    let gate = Arc::new(Semaphore::new(max_parallel));
    let total = instances.len();
    let mut set = JoinSet::new();
    for (index, (r, combo)) in instances.into_iter().enumerate() {
        let strategy_ctx = json!({
            "fail-fast": fail_fast,
            "job-index": index,
            "job-total": total,
            "max-parallel": max_parallel,
        });
        let inst = job::Instance {
            engine: engine.clone(),
            spec: spec.clone(),
            job_id: id.to_string(),
            job: job.clone(),
            row: r,
            matrix: combo,
            strategy: strategy_ctx,
            needs: needs.clone(),
            cancel: group_cancel.child_token(),
        };
        let (gate, group_cancel) = (gate.clone(), group_cancel.clone());
        set.spawn(async move {
            let _permit = tokio::select! {
                p = gate.acquire_owned() => p.ok(),
                _ = group_cancel.cancelled() => None,
            };
            let (status, outputs) = if group_cancel.is_cancelled() {
                let _ = inst.engine.db.finish_job(inst.row, Status::Cancelled, None);
                (Status::Cancelled, Map::new())
            } else {
                job::run(inst).await
            };
            if status == Status::Failed && fail_fast {
                group_cancel.cancel();
            }
            (status, outputs)
        });
    }
    let mut statuses = Vec::new();
    let mut outputs = Map::new();
    while let Some(r) = set.join_next().await {
        if let Ok((st, out)) = r {
            statuses.push(st);
            outputs.extend(out);
        }
    }
    let result = if statuses.contains(&Status::Failed) {
        if continue_on_error { "success" } else { "failure" }
    } else if statuses.contains(&Status::Cancelled) {
        if spec.cancel.is_cancelled() { "cancelled" } else { "failure" }
    } else {
        "success"
    };
    JobResult { result, outputs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_on_images() {
        let cfg = GhaConfig {
            images: GhaConfig::parse_images("self-hosted=my/runner:1, ubuntu-latest = x/y").unwrap(),
            github_token: None,
        };
        assert_eq!(cfg.image_for(&["ubuntu-latest".into()]).unwrap().0, "x/y");
        assert_eq!(cfg.image_for(&["self-hosted".into(), "linux".into()]).unwrap().0, "my/runner:1");
        let d = GhaConfig::default();
        assert_eq!(d.image_for(&["ubuntu-22.04".into()]).unwrap().0, "moloko/ubuntu:22.04-1");
        let df = builtin_dockerfile("moloko/ubuntu:22.04-1").unwrap();
        assert!(df.starts_with("FROM catthehacker/ubuntu:act-22.04\n") && df.contains("xdg-utils"));
        assert!(builtin_dockerfile("catthehacker/ubuntu:act-22.04").is_none());
        let (img, note) = d.image_for(&["gpu".into()]).unwrap();
        assert_eq!(img, "moloko/ubuntu:latest-1");
        assert!(note.unwrap().contains("gpu"));
        if cfg!(target_os = "macos") {
            assert_eq!(d.image_for(&["macos-14".into()]).unwrap().0, "host");
        } else {
            assert!(d.image_for(&["macos-14".into()]).is_err());
        }
        assert_eq!(d.image_for(&["windows-latest".into()]).unwrap().0, WINDOWS_CROSS_IMAGE);
        assert!(builtin_dockerfile(WINDOWS_CROSS_IMAGE).unwrap().contains("cargo-xwin"));
        assert!(GhaConfig::parse_images("nope").is_err());
    }
}
