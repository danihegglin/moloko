//! HTTP API, GitHub webhook, live log streaming (SSE) and the embedded dashboard.

use crate::engine::Engine;
use crate::logs::Subscription;
use axum::body::Bytes;
use axum::extract::Request;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Extension, Json, Router};
use crate::auth::{Actor, Role};
use futures_util::Stream;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub webhook_secret: Option<String>,
    pub wg: Option<WgServer>,
}

/// The server's built-in WireGuard endpoint.
#[derive(Clone)]
pub struct WgServer {
    pub netstack: crate::tunnel::Netstack,
    pub public: [u8; 32],
    /// host:port agents dial, written into join codes.
    pub endpoint: String,
}

impl WgServer {
    pub fn peer(agent_id: i64, public: [u8; 32]) -> crate::tunnel::PeerConfig {
        crate::tunnel::PeerConfig {
            index: agent_id as u32,
            public,
            ip: crate::tunnel::agent_ip(agent_id),
            endpoint: None,
            keepalive: None,
        }
    }
}

pub fn router(state: AppState) -> Router {
    // Everything under /api except login, webhooks and the agent socket needs a user.
    let protected = Router::new()
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/{id}", get(get_project).patch(update_project))
        .route("/api/projects/{id}/runs", post(trigger_run))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/jobs/{id}/logs", get(job_logs))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route("/api/agents/{id}", delete(delete_agent))
        .route("/api/agents/{id}/join", post(rotate_join))
        .route("/api/images", get(list_images))
        .merge(crate::users_api::routes())
        .merge(crate::code::routes())
        .merge(crate::issues::routes())
        .merge(crate::reviews::routes())
        .merge(crate::ai::routes())
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_user));
    Router::new()
        .merge(protected)
        .merge(crate::users_api::public_routes())
        .route("/api/webhooks/github", post(github_webhook))
        .route("/api/agents/connect", get(agent_connect))
        .route("/v2", any(registry))
        .route("/v2/", any(registry))
        .route("/v2/{*rest}", any(registry))
        .merge(crate::githttp::routes())
        .fallback(static_files)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}

async fn require_user(
    State(s): State<AppState>,
    mut req: Request,
    next: axum::middleware::Next,
) -> Response {
    match s.engine.db.authenticate(req.headers()) {
        Ok(Some(actor)) => {
            req.extensions_mut().insert(actor);
            next.run(req).await
        }
        Ok(None) => ApiError(StatusCode::UNAUTHORIZED, "login required".into()).into_response(),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// Check the actor has at least `min` in the project. Without any role the project is hidden (404).
pub(crate) fn need(s: &AppState, actor: &Actor, project_id: i64, min: Role) -> ApiResult<Role> {
    if let Some(scope) = &actor.scope
        && scope.project_id.is_some_and(|p| p != project_id)
    {
        return Err(not_found());
    }
    match s.engine.db.role(&actor.user, project_id)? {
        None => Err(not_found()),
        Some(r) if r < min => Err(ApiError(
            StatusCode::FORBIDDEN,
            format!("needs {} access to this project", min.as_str()),
        )),
        Some(r) => Ok(r),
    }
}

pub(crate) fn need_admin(actor: &Actor) -> ApiResult<()> {
    if actor.user.is_admin && actor.scope.is_none() {
        Ok(())
    } else {
        Err(ApiError(StatusCode::FORBIDDEN, "admins only".into()))
    }
}

/// Look up a repo and check the actor's role in its project.
pub(crate) fn repo_for(
    s: &AppState,
    actor: &Actor,
    repo_id: i64,
    min: Role,
) -> ApiResult<crate::repos::Repo> {
    let repo = s.engine.db.repo(repo_id)?.ok_or_else(not_found)?;
    need(s, actor, repo.project_id, min)?;
    Ok(repo)
}

pub(crate) struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

pub(crate) fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

pub(crate) fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found".into())
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// Projects the actor can see.
pub(crate) fn visible_projects(s: &AppState, actor: &Actor) -> ApiResult<Vec<crate::db::Project>> {
    let mut out = Vec::new();
    for p in s.engine.db.projects()? {
        if need(s, actor, p.id, Role::Read).is_ok() {
            out.push(p);
        }
    }
    Ok(out)
}

async fn list_projects(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(visible_projects(&s, &actor)?))
}

#[derive(Deserialize)]
struct NewProject {
    name: String,
    /// Optional: import this repo and mirror it (`mirror_mode`, default `pull`).
    #[serde(default)]
    repo_url: String,
    key: Option<String>,
    mirror_mode: Option<String>,
    credential: Option<String>,
}

async fn create_project(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(p): Json<NewProject>,
) -> ApiResult<impl IntoResponse> {
    if actor.scope.is_some() || actor.user.kind != "human" {
        return Err(ApiError(StatusCode::FORBIDDEN, "only people can create projects".into()));
    }
    let (name, url) = (p.name.trim(), p.repo_url.trim());
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let id = s
        .engine
        .db
        .create_project(name, url)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    if let Some(key) = p.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        if !crate::issues::valid_key(key) {
            return Err(bad_request("key must be 1-10 uppercase letters or digits, starting with a letter"));
        }
        s.engine.db.set_project_key(id, key)?;
    }
    s.engine.db.set_member(id, actor.user.id, Role::Admin)?;
    if let Some(repo) = s.engine.db.repos(id)?.into_iter().next() {
        let mode = p.mirror_mode.as_deref().and_then(crate::repos::MirrorMode::parse);
        s.engine.db.update_repo(repo.id, crate::repos::RepoSettings {
            mirror_mode: mode,
            credential: p.credential.filter(|c| !c.is_empty()).map(|c| Some(s.engine.secrets.seal(&c))),
            ..Default::default()
        })?;
        // Import in the background; a mirror can take a while.
        let engine = s.engine.clone();
        tokio::spawn(async move {
            if let Ok(Some(repo)) = engine.db.repo(repo.id) {
                let r = if repo.mirror_mode == crate::repos::MirrorMode::Pull {
                    crate::mirror::sync(&engine, &repo).await.map(|_| ())
                } else {
                    crate::code::import(&engine, &repo).await
                };
                if let Err(e) = r {
                    tracing::warn!(repo = repo.name, "import failed: {e:#}");
                }
            }
        });
    }
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

async fn get_project(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let role = need(&s, &actor, id, Role::Read)?;
    let project = s.engine.db.project(id)?.ok_or_else(not_found)?;
    let repos = s.engine.db.repos(id)?;
    let members: Vec<_> = s
        .engine
        .db
        .members(id)?
        .into_iter()
        .map(|(u, r)| json!({ "user": u, "role": r }))
        .collect();
    Ok(Json(json!({ "project": project, "repos": repos, "role": role, "members": members })))
}

#[derive(Deserialize)]
struct ProjectPatch {
    key: Option<String>,
}

async fn update_project(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(p): Json<ProjectPatch>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    if let Some(key) = p.key {
        if !crate::issues::valid_key(&key) {
            return Err(bad_request("key must be 1-10 uppercase letters or digits, starting with a letter"));
        }
        s.engine.db.set_project_key(id, &key)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct TriggerBody {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    repo_id: Option<i64>,
}

async fn trigger_run(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    body: Option<Json<TriggerBody>>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Write)?;
    let body = body.map(|b| b.0);
    let repos = s.engine.db.repos(id)?;
    let repo = match body.as_ref().and_then(|b| b.repo_id) {
        Some(rid) => repos.into_iter().find(|r| r.id == rid),
        None => repos.into_iter().next(),
    }
    .ok_or_else(|| bad_request("project has no such repo"))?;
    let git_ref = body
        .and_then(|b| b.git_ref)
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "HEAD".into());
    let run_id = s.engine.trigger(repo.id, git_ref.trim(), None, "manual")?;
    Ok((StatusCode::CREATED, Json(json!({ "id": run_id }))))
}

#[derive(Deserialize)]
struct RunsQuery {
    project_id: Option<i64>,
    limit: Option<i64>,
}

async fn list_runs(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<impl IntoResponse> {
    if let Some(p) = q.project_id {
        need(&s, &actor, p, Role::Read)?;
    }
    let visible: std::collections::HashSet<i64> =
        visible_projects(&s, &actor)?.into_iter().map(|p| p.id).collect();
    let runs: Vec<_> = s
        .engine
        .db
        .runs(q.project_id, q.limit.unwrap_or(50).clamp(1, 500))?
        .into_iter()
        .filter(|r| visible.contains(&r.project_id))
        .collect();
    Ok(Json(runs))
}

async fn get_run(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let run = s.engine.db.run(id)?.ok_or_else(not_found)?;
    need(&s, &actor, run.project_id, Role::Read)?;
    let jobs = s.engine.db.jobs(id)?;
    Ok(Json(json!({ "run": run, "jobs": jobs })))
}

async fn cancel_run(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let run = s.engine.db.run(id)?.ok_or_else(not_found)?;
    need(&s, &actor, run.project_id, Role::Write)?;
    if s.engine.cancel(id) {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError(StatusCode::CONFLICT, "run is not active".into()))
    }
}

/// Server-sent events: one `line` event per log line, then `end`.
async fn job_logs(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let project_id = s.engine.db.job_project(id)?.ok_or_else(not_found)?;
    need(&s, &actor, project_id, Role::Read)?;
    let sub = s.engine.logs.subscribe(id);
    let stream = async_stream::stream! {
        let (existing, rx) = match sub {
            Subscription::Live(content, rx) => (content, Some(rx)),
            Subscription::Done(content) => (content, None),
        };
        for line in existing.lines() {
            yield Ok(Event::default().event("line").data(line));
        }
        if let Some(mut rx) = rx {
            loop {
                match rx.recv().await {
                    Ok(line) => yield Ok(Event::default().event("line").data(line)),
                    Err(RecvError::Lagged(n)) => yield Ok(Event::default().event("line").data(format!("… {n} lines skipped (reload to see all)"))),
                    Err(RecvError::Closed) => break,
                }
            }
        }
        yield Ok(Event::default().event("end").data(""));
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn list_agents(State(s): State<AppState>) -> ApiResult<impl IntoResponse> {
    let online = s.engine.agents.presence();
    let agents: Vec<_> = s
        .engine
        .db
        .agents()?
        .into_iter()
        .map(|a| {
            let presence = online.get(&a.id).cloned();
            json!({ "agent": a, "online": presence.is_some(), "presence": presence })
        })
        .collect();
    Ok(Json(agents))
}

#[derive(Deserialize)]
struct NewAgent {
    name: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// Register an agent. The token is returned once and only its hash is stored.
async fn create_agent(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(a): Json<NewAgent>,
) -> ApiResult<impl IntoResponse> {
    need_admin(&actor)?;
    let name = a.name.trim();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let labels: Vec<String> = a
        .labels
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let token = crate::agents::new_token();
    let id = s
        .engine
        .db
        .create_agent(name, &labels, &crate::agents::hash_token(&token))
        .map_err(|e| bad_request(format!("{e:#}")))?;
    // With WireGuard on, the agent gets a key pair and a join code that carries everything.
    let join = match &s.wg {
        Some(wg) => Some(issue_join(&s, wg, id, &token)?),
        None => None,
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "token": token, "join": join })),
    ))
}

/// Give agent `id` a fresh WireGuard key pair and return its join code.
fn issue_join(s: &AppState, wg: &WgServer, id: i64, token: &str) -> anyhow::Result<String> {
    let private = crate::tunnel::new_private_key();
    let public = crate::tunnel::public_key(&private);
    if let Some((_, old)) = s
        .engine
        .db
        .agent_wg_keys()?
        .into_iter()
        .find(|(a, _)| *a == id)
        && let Ok(k) = crate::tunnel::key_from_b64(&old)
    {
        wg.netstack.remove_peer(k);
    }
    s.engine
        .db
        .set_agent_wg_public(id, &crate::tunnel::b64(&public))?;
    wg.netstack.add_peer(WgServer::peer(id, public));
    Ok(crate::tunnel::JoinCode {
        endpoint: wg.endpoint.clone(),
        server_public: crate::tunnel::b64(&wg.public),
        private: crate::tunnel::b64(&private),
        ip: crate::tunnel::agent_ip(id),
        token: token.to_string(),
    }
    .encode())
}

/// Issue a new join code (new WireGuard key and token) for an existing agent.
/// The old code stops working and the agent's current connection is closed.
async fn rotate_join(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    need_admin(&actor)?;
    let Some(wg) = &s.wg else {
        return Err(bad_request("WireGuard is disabled on this server"));
    };
    if !s.engine.db.agents()?.iter().any(|a| a.id == id) {
        return Err(not_found());
    }
    let token = crate::agents::new_token();
    s.engine
        .db
        .set_agent_token_hash(id, &crate::agents::hash_token(&token))?;
    let join = issue_join(&s, wg, id, &token)?;
    s.engine.agents.disconnect(id);
    Ok(Json(json!({ "id": id, "token": token, "join": join })))
}

async fn delete_agent(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    need_admin(&actor)?;
    let wg_key = s
        .engine
        .db
        .agent_wg_keys()?
        .into_iter()
        .find(|(a, _)| *a == id);
    if !s.engine.db.delete_agent(id)? {
        return Err(not_found());
    }
    if let (Some(wg), Some((_, key))) = (&s.wg, wg_key)
        && let Ok(k) = crate::tunnel::key_from_b64(&key)
    {
        wg.netstack.remove_peer(k);
    }
    s.engine.agents.disconnect(id);
    Ok(StatusCode::NO_CONTENT)
}

/// Agents dial in here with `Authorization: Bearer <token>`.
async fn agent_connect(
    State(s): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "missing bearer token".into()))?;
    let agent = s
        .engine
        .db
        .agent_by_token(&crate::agents::hash_token(token))?
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "unknown agent token".into()))?;
    let engine = s.engine.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        engine.agents.serve(socket, agent, engine.db.clone()).await
    }))
}

async fn list_images(State(s): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(json!({
        "registry": s.engine.registry_host,
        "repositories": s.engine.registry.images(),
    })))
}

/// OCI distribution API. Run tokens can push, agent tokens can pull.
async fn registry(State(s): State<AppState>, req: Request) -> Response {
    let db = s.engine.db.clone();
    let access = s.engine.registry.authorize(req.headers(), |token| {
        db.agent_by_token(&crate::agents::hash_token(token))
            .ok()
            .flatten()
            .map(|a| a.name)
    });
    s.engine.registry.handle(req, access).await
}

/// Reduce clone/html URLs to `host/owner/repo` so ssh and https forms of one repo match.
pub fn normalize_repo_url(url: &str) -> String {
    let u = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    let u = u.split("://").last().unwrap_or(&u).to_string();
    let u = u.rsplit('@').next().unwrap_or(&u).to_string();
    // scp-style git@host:owner/repo
    match u.split_once(':') {
        Some((host, rest)) if !rest.starts_with(|c: char| c.is_ascii_digit()) => {
            format!("{host}/{rest}")
        }
        Some((host, rest)) => {
            // host:port/path
            let path = rest.split_once('/').map(|x| x.1).unwrap_or("");
            format!("{host}/{path}")
        }
        None => u,
    }
}

pub fn verify_signature(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(hex_sig) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(sig) = hex::decode(hex_sig) else {
        return false;
    };
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

#[derive(Deserialize)]
struct PushEvent {
    repository: PushRepo,
}

#[derive(Deserialize)]
struct PushRepo {
    clone_url: Option<String>,
    ssh_url: Option<String>,
    html_url: Option<String>,
}

async fn github_webhook(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let Some(secret) = &s.webhook_secret else {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "webhooks disabled: set MOLOKO_WEBHOOK_SECRET".into(),
        ));
    };
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    if !verify_signature(secret, &body, sig) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "bad signature".into()));
    }
    match headers.get("x-github-event").and_then(|v| v.to_str().ok()) {
        Some("ping") => return Ok(Json(json!({ "ok": true, "synced": [] }))),
        Some("push") => {}
        other => return Err(bad_request(format!("unsupported event {other:?}"))),
    }
    let ev: PushEvent = serde_json::from_slice(&body).map_err(|e| bad_request(e.to_string()))?;
    let urls: Vec<String> = [
        &ev.repository.clone_url,
        &ev.repository.ssh_url,
        &ev.repository.html_url,
    ]
    .into_iter()
    .flatten()
    .map(|u| normalize_repo_url(u))
    .collect();
    // Mirrored repos fetch the change; the fetch starts runs and updates issues and reviews.
    let mut synced = Vec::new();
    for repo in s.engine.db.all_repos()? {
        let Some(up) = &repo.upstream_url else { continue };
        if repo.mirror_mode == crate::repos::MirrorMode::None || !urls.contains(&normalize_repo_url(up)) {
            continue;
        }
        synced.push(repo.id);
        let engine = s.engine.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::mirror::sync(&engine, &repo).await {
                tracing::warn!(repo = repo.name, "webhook sync failed: {e:#}");
            }
        });
    }
    Ok(Json(json!({ "ok": true, "synced": synced })))
}

#[derive(rust_embed::Embed)]
#[folder = "../dashboard/dist"]
struct Dashboard;

async fn static_files(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return not_found().into_response();
    }
    // Unknown paths fall back to index.html so client-side routes work.
    let (file, name) = match Dashboard::get(path) {
        Some(f) if !path.is_empty() => (f, path),
        _ => match Dashboard::get("index.html") {
            Some(f) => (f, "index.html"),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "dashboard not built: run `make dashboard`",
                )
                    .into_response();
            }
        },
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        file.data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls_normalize_across_forms() {
        let want = "github.com/acme/api";
        for u in [
            "https://github.com/acme/api.git",
            "https://github.com/Acme/API",
            "git@github.com:acme/api.git",
            "ssh://git@github.com/acme/api.git",
            "ssh://git@github.com:22/acme/api.git",
        ] {
            assert_eq!(normalize_repo_url(u), want, "{u}");
        }
    }

    #[test]
    fn webhook_signature() {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(b"s3cret").unwrap();
        mac.update(b"payload");
        let good = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature("s3cret", b"payload", Some(&good)));
        assert!(!verify_signature("other", b"payload", Some(&good)));
        assert!(!verify_signature("s3cret", b"payload", None));
        assert!(!verify_signature("s3cret", b"payload", Some("sha256=zz")));
    }
}
