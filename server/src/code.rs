//! Repo settings, mirroring controls and code browsing (branches, tree, files, commits, diffs).

use crate::api::{ApiError, ApiResult, AppState, bad_request, need, not_found, repo_for};
use crate::auth::{Actor, Role};
use crate::engine::Engine;
use crate::git::{self, git};
use crate::repos::{MirrorMode, Repo, RepoSettings};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects/{id}/repos", post(create_repo))
        .route("/api/repos/{id}", get(get_repo).patch(update_repo).delete(delete_repo))
        .route("/api/repos/{id}/mirror", get(mirror_state))
        .route("/api/repos/{id}/sync", post(sync_now))
        .route("/api/repos/{id}/mirror/resolve", post(resolve))
        .route("/api/repos/{id}/branches", get(branches).post(create_branch))
        .route("/api/repos/{id}/tree", get(tree))
        .route("/api/repos/{id}/blob", get(blob))
        .route("/api/repos/{id}/commits", get(commits))
        .route("/api/repos/{id}/commits/{sha}", get(commit))
        .route("/api/repos/{id}/compare", get(compare))
}

/// Refs and paths from requests must not be mistaken for options.
pub fn safe_ref(r: &str) -> ApiResult<&str> {
    if r.is_empty() || r.starts_with('-') || r.contains("..") || r.chars().any(|c| c.is_control() || c == ' ') {
        return Err(bad_request(format!("bad ref {r:?}")));
    }
    Ok(r)
}

fn safe_path(p: &str) -> ApiResult<String> {
    let p = p.trim_matches('/');
    if p.split('/').any(|c| c == "..") {
        return Err(bad_request("bad path"));
    }
    Ok(p.to_string())
}

/// One-time copy of upstream's branches and tags, for repos that become the source of truth.
pub async fn import(engine: &Arc<Engine>, repo: &Repo) -> Result<()> {
    let Some(url) = repo.upstream_url.clone().filter(|u| !u.is_empty()) else {
        return Ok(());
    };
    let path = engine.git.ensure(repo.id, &repo.default_branch).await?;
    let env = crate::mirror::cred_env(engine, repo)?;
    git::git_env(
        &["fetch", "--quiet", "--no-write-fetch-head", &url, "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"],
        Some(&path),
        &env,
    )
    .await?;
    if matches!(repo.mirror_mode, MirrorMode::Push | MirrorMode::Twoway) {
        for (name, sha) in engine.git.refs(&path).await? {
            engine.db.set_mirror_ref(repo.id, &name, Some(&sha), Some(&sha), "in_sync", None)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    upstream_url: Option<String>,
    mirror_mode: Option<String>,
    credential: Option<String>,
}

async fn create_repo(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(project_id): Path<i64>,
    Json(r): Json<NewRepo>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, project_id, Role::Admin)?;
    let url = r.upstream_url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let mode = match r.mirror_mode.as_deref() {
        Some(m) => MirrorMode::parse(m).ok_or_else(|| bad_request("mirror_mode: none, pull, push or twoway"))?,
        // With a URL, moloko becomes primary and keeps GitHub as a read-only copy.
        None if url.is_some() => MirrorMode::Push,
        None => MirrorMode::None,
    };
    let id = s
        .engine
        .db
        .create_repo(project_id, r.name.trim(), url, mode)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    if let Some(c) = r.credential.filter(|c| !c.is_empty()) {
        s.engine.db.update_repo(id, RepoSettings { credential: Some(Some(s.engine.secrets.seal(&c))), ..Default::default() })?;
    }
    let repo = s.engine.db.repo(id)?.ok_or_else(not_found)?;
    s.engine.git.ensure(repo.id, &repo.default_branch).await?;
    if url.is_some() {
        let engine = s.engine.clone();
        let repo2 = repo.clone();
        tokio::spawn(async move {
            let r = if repo2.mirror_mode == MirrorMode::Pull {
                crate::mirror::sync(&engine, &repo2).await.map(|_| ())
            } else {
                import(&engine, &repo2).await
            };
            if let Err(e) = r {
                tracing::warn!(repo = repo2.name, "import failed: {e:#}");
            }
        });
    }
    Ok((StatusCode::CREATED, Json(repo)))
}

async fn get_repo(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let project = s.engine.db.project(repo.project_id)?.ok_or_else(not_found)?;
    let clone_path = format!("/git/{}/{}.git", project.name, repo.name);
    Ok(Json(json!({ "repo": repo, "clone_path": clone_path })))
}

#[derive(Deserialize)]
struct RepoPatch {
    default_branch: Option<String>,
    upstream_url: Option<String>,
    mirror_mode: Option<String>,
    on_unreachable: Option<String>,
    /// Empty string removes the credential.
    credential: Option<String>,
    protect_default: Option<bool>,
    require_review: Option<bool>,
}

async fn update_repo(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(p): Json<RepoPatch>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Admin)?;
    let mode = match p.mirror_mode.as_deref() {
        Some(m) => Some(MirrorMode::parse(m).ok_or_else(|| bad_request("mirror_mode: none, pull, push or twoway"))?),
        None => None,
    };
    if let Some(o) = &p.on_unreachable
        && o != "reject"
        && o != "queue"
    {
        return Err(bad_request("on_unreachable: reject or queue"));
    }
    if let Some(b) = &p.default_branch {
        safe_ref(b)?;
    }
    let url = p.upstream_url.map(|u| Some(u.trim().to_string()).filter(|u| !u.is_empty()));
    let effective_url = url.clone().unwrap_or(repo.upstream_url.clone());
    if mode.unwrap_or(repo.mirror_mode) != MirrorMode::None && effective_url.is_none() {
        return Err(bad_request("mirroring needs an upstream URL"));
    }
    s.engine.db.update_repo(id, RepoSettings {
        default_branch: p.default_branch.clone(),
        upstream_url: url,
        mirror_mode: mode,
        on_unreachable: p.on_unreachable,
        credential: p.credential.map(|c| (!c.is_empty()).then(|| s.engine.secrets.seal(&c))),
        protect_default: p.protect_default,
        require_review: p.require_review,
    })?;
    if let Some(b) = p.default_branch {
        let path = s.engine.git.ensure(id, &b).await?;
        git(&["symbolic-ref", "HEAD", &format!("refs/heads/{b}")], Some(&path)).await?;
    }
    Ok(Json(s.engine.db.repo(id)?))
}

async fn delete_repo(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    repo_for(&s, &actor, id, Role::Admin)?;
    s.engine.db.delete_repo(id)?;
    let _ = tokio::fs::remove_dir_all(s.engine.git.path(id)).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn mirror_state(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    repo_for(&s, &actor, id, Role::Read)?;
    Ok(Json(s.engine.db.mirror_refs(id)?))
}

async fn sync_now(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Write)?;
    let updates = crate::mirror::sync(&s.engine, &repo)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "updated": updates.iter().map(|u| &u.name).collect::<Vec<_>>() })))
}

#[derive(Deserialize)]
struct Resolve {
    #[serde(rename = "ref")]
    name: String,
    keep: String,
}

async fn resolve(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(r): Json<Resolve>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Admin)?;
    crate::mirror::resolve_diverged(&s.engine, &repo, &r.name, &r.keep)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct Branch {
    name: String,
    sha: String,
    subject: String,
    date: i64,
    author: String,
    run: Option<crate::db::Run>,
    mirror: Option<String>,
}

async fn branches(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let path = s.engine.git.ensure(id, &repo.default_branch).await?;
    let out = git(
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%00%(objectname)%00%(contents:subject)%00%(committerdate:unix)%00%(authorname)",
            "refs/heads",
            "refs/tags",
        ],
        Some(&path),
    )
    .await?;
    let mirror: std::collections::HashMap<String, String> = s
        .engine
        .db
        .mirror_refs(id)?
        .into_iter()
        .map(|m| (m.name, m.state))
        .collect();
    let mut list = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split('\0').collect();
        if f.len() < 5 {
            continue;
        }
        list.push(Branch {
            name: f[0].to_string(),
            sha: f[1].to_string(),
            subject: f[2].to_string(),
            date: f[3].parse::<i64>().unwrap_or(0) * 1000,
            author: f[4].to_string(),
            run: s.engine.db.run_for_sha(id, f[1])?,
            mirror: mirror.get(f[0]).cloned(),
        });
    }
    Ok(Json(json!({ "default_branch": repo.default_branch, "refs": list })))
}

#[derive(Deserialize)]
struct NewBranch {
    name: String,
    from: Option<String>,
}

async fn create_branch(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(b): Json<NewBranch>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Write)?;
    let name = safe_ref(b.name.trim())?.to_string();
    let path = s.engine.git.ensure(id, &repo.default_branch).await?;
    let from = b.from.unwrap_or_else(|| repo.default_branch.clone());
    let sha = s.engine.git.resolve(&path, safe_ref(&from)?).await.map_err(|e| bad_request(format!("{e:#}")))?;
    let results = s
        .engine
        .apply_ref_updates(
            &repo,
            Some(&actor),
            &[crate::repos::RefUpdate { name: format!("refs/heads/{name}"), old: crate::repos::zero(), new: sha.clone() }],
            crate::repos::Source::Push,
            true,
        )
        .await?;
    match results.into_iter().next() {
        Some(Err(e)) => Err(ApiError(StatusCode::CONFLICT, e)),
        _ => Ok((StatusCode::CREATED, Json(json!({ "name": name, "sha": sha })))),
    }
}

#[derive(Deserialize)]
struct TreeQuery {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    path: Option<String>,
}

async fn resolve_ref(s: &AppState, repo: &Repo, r: Option<&str>) -> ApiResult<(std::path::PathBuf, String)> {
    let path = s.engine.git.ensure(repo.id, &repo.default_branch).await?;
    let r = r.unwrap_or(&repo.default_branch);
    let sha = s
        .engine
        .git
        .resolve(&path, safe_ref(r)?)
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, format!("ref {r} not found")))?;
    Ok((path, sha))
}

async fn tree(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<TreeQuery>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let (path, sha) = resolve_ref(&s, &repo, q.git_ref.as_deref()).await?;
    let sub = safe_path(q.path.as_deref().unwrap_or(""))?;
    let spec = if sub.is_empty() { sha.clone() } else { format!("{sha}:{sub}") };
    let out = git(&["ls-tree", "-l", "-z", &spec], Some(&path))
        .await
        .map_err(|_| not_found())?;
    let mut entries: Vec<serde_json::Value> = out
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let (meta, name) = l.split_once('\t')?;
            let m: Vec<&str> = meta.split_whitespace().collect();
            let kind = *m.get(1)?;
            let size = m.get(3).and_then(|x| x.parse::<i64>().ok());
            let full = if sub.is_empty() { name.to_string() } else { format!("{sub}/{name}") };
            Some(json!({ "name": name, "type": kind, "path": full, "size": size }))
        })
        .collect();
    entries.sort_by_key(|e| (e["type"] != "tree", e["name"].as_str().unwrap_or("").to_lowercase()));
    let readme = entries
        .iter()
        .find(|e| e["type"] == "blob" && e["name"].as_str().is_some_and(|n| n.to_lowercase().starts_with("readme")))
        .and_then(|e| e["path"].as_str().map(str::to_string));
    let readme = match readme {
        Some(p) => s.engine.git.read_file(&path, &sha, &p).await.ok().map(|c| json!({ "path": p, "content": c })),
        None => None,
    };
    Ok(Json(json!({ "sha": sha, "path": sub, "entries": entries, "readme": readme })))
}

async fn blob(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<TreeQuery>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let (path, sha) = resolve_ref(&s, &repo, q.git_ref.as_deref()).await?;
    let file = safe_path(q.path.as_deref().unwrap_or(""))?;
    let out = git::git_output(&["cat-file", "blob", &format!("{sha}:{file}")], Some(&path), &[], None).await?;
    if !out.status.success() {
        return Err(not_found());
    }
    let size = out.stdout.len();
    let binary = out.stdout.iter().take(8000).any(|&b| b == 0);
    const MAX: usize = 1 << 20;
    let content = if binary || size > MAX {
        None
    } else {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    };
    Ok(Json(json!({ "sha": sha, "path": file, "size": size, "binary": binary, "content": content })))
}

#[derive(Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub date: i64,
    pub subject: String,
    pub body: String,
}

const LOG_FORMAT: &str = "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%s%x00%b%x1e";

fn parse_log(out: &str) -> Vec<CommitInfo> {
    out.split('\x1e')
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.splitn(7, '\0').collect();
            (f.len() == 7).then(|| CommitInfo {
                sha: f[0].into(),
                parents: f[1].split_whitespace().map(str::to_string).collect(),
                author: f[2].into(),
                email: f[3].into(),
                date: f[4].parse::<i64>().unwrap_or(0) * 1000,
                subject: f[5].into(),
                body: f[6].trim_end().into(),
            })
        })
        .collect()
}

/// Commits in `range` (anything `git log` takes), newest first.
pub async fn log(path: &std::path::Path, range: &str, limit: usize) -> Result<Vec<CommitInfo>> {
    let n = format!("-n{limit}");
    Ok(parse_log(&git(&["log", &n, LOG_FORMAT, range, "--"], Some(path)).await?))
}

#[derive(Deserialize)]
struct CommitsQuery {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
}

async fn commits(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<CommitsQuery>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let (path, sha) = resolve_ref(&s, &repo, q.git_ref.as_deref()).await?;
    let n = format!("-n{}", q.limit.unwrap_or(50).clamp(1, 500));
    let mut args = vec!["log", &n, LOG_FORMAT, &sha, "--"];
    let sub = safe_path(q.path.as_deref().unwrap_or(""))?;
    if !sub.is_empty() {
        args.push(&sub);
    }
    let list = parse_log(&git(&args, Some(&path)).await?);
    let with_runs: Vec<_> = list
        .into_iter()
        .map(|c| {
            let run = s.engine.db.run_for_sha(id, &c.sha).ok().flatten();
            json!({ "commit": c, "run": run })
        })
        .collect();
    Ok(Json(with_runs))
}

#[derive(Serialize)]
pub struct FileStat {
    pub path: String,
    pub added: Option<i64>,
    pub removed: Option<i64>,
}

/// Unified diff plus per-file stats between two commits.
pub async fn diff(path: &std::path::Path, from: &str, to: &str) -> Result<(Vec<FileStat>, String)> {
    let stat = git(&["diff", "--numstat", "-M", from, to, "--"], Some(path)).await?;
    let files = stat
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            let (a, r, f) = (p.next()?, p.next()?, p.next()?);
            Some(FileStat { path: f.into(), added: a.parse().ok(), removed: r.parse().ok() })
        })
        .collect();
    let out = git::git_output(&["diff", "-M", "--no-color", from, to, "--"], Some(path), &[], None).await?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    const MAX: usize = 4 << 20;
    if text.len() > MAX {
        let cut = (0..=MAX).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… diff truncated\n");
    }
    Ok((files, text))
}

async fn commit(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, sha)): Path<(i64, String)>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let (path, sha) = resolve_ref(&s, &repo, Some(&sha)).await?;
    let info = log(&path, &sha, 1).await?.into_iter().next().ok_or_else(not_found)?;
    // Root commits diff against the empty tree.
    let parent = info.parents.first().cloned().unwrap_or_else(|| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into());
    let (files, text) = diff(&path, &parent, &sha).await?;
    let run = s.engine.db.run_for_sha(id, &sha)?;
    Ok(Json(json!({ "commit": info, "files": files, "diff": text, "run": run })))
}

#[derive(Deserialize)]
struct CompareQuery {
    base: String,
    head: String,
}

async fn compare(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<CompareQuery>,
) -> ApiResult<impl IntoResponse> {
    let repo = repo_for(&s, &actor, id, Role::Read)?;
    let (path, base) = resolve_ref(&s, &repo, Some(&q.base)).await?;
    let (_, head) = resolve_ref(&s, &repo, Some(&q.head)).await?;
    let mb = git(&["merge-base", &base, &head], Some(&path)).await.map_err(|_| bad_request("no common history"))?;
    let commits = log(&path, &format!("{mb}..{head}"), 250).await?;
    let (files, text) = diff(&path, &mb, &head).await?;
    Ok(Json(json!({ "merge_base": mb, "base": base, "head": head, "commits": commits, "files": files, "diff": text })))
}
