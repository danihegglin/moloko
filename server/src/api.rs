//! HTTP API, GitHub webhook, live log streaming (SSE) and the embedded dashboard.

use crate::engine::Engine;
use crate::logs::Subscription;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
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
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/{id}/runs", post(trigger_run))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/jobs/{id}/logs", get(job_logs))
        .route("/api/webhooks/github", post(github_webhook))
        .fallback(static_files)
        .with_state(state)
}

struct ApiError(StatusCode, String);

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

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found".into())
}

type ApiResult<T> = Result<T, ApiError>;

async fn list_projects(State(s): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(s.engine.db.projects()?))
}

#[derive(Deserialize)]
struct NewProject {
    name: String,
    repo_url: String,
}

async fn create_project(
    State(s): State<AppState>,
    Json(p): Json<NewProject>,
) -> ApiResult<impl IntoResponse> {
    let (name, url) = (p.name.trim(), p.repo_url.trim());
    if name.is_empty() || url.is_empty() {
        return Err(bad_request("name and repo_url are required"));
    }
    let id = s
        .engine
        .db
        .create_project(name, url)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
struct TriggerBody {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
}

async fn trigger_run(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<TriggerBody>>,
) -> ApiResult<impl IntoResponse> {
    if s.engine.db.project(id)?.is_none() {
        return Err(not_found());
    }
    let git_ref = body
        .and_then(|b| b.0.git_ref)
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "HEAD".into());
    let run_id = s.engine.trigger(id, git_ref.trim(), None, "manual")?;
    Ok((StatusCode::CREATED, Json(json!({ "id": run_id }))))
}

#[derive(Deserialize)]
struct RunsQuery {
    project_id: Option<i64>,
    limit: Option<i64>,
}

async fn list_runs(
    State(s): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        s.engine
            .db
            .runs(q.project_id, q.limit.unwrap_or(50).clamp(1, 500))?,
    ))
}

async fn get_run(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    let run = s.engine.db.run(id)?.ok_or_else(not_found)?;
    let jobs = s.engine.db.jobs(id)?;
    Ok(Json(json!({ "run": run, "jobs": jobs })))
}

async fn cancel_run(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    if s.engine.cancel(id) {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError(StatusCode::CONFLICT, "run is not active".into()))
    }
}

/// Server-sent events: one `line` event per log line, then `end`.
async fn job_logs(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    if !s.engine.db.job_exists(id)? {
        return Err(not_found());
    }
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
    #[serde(rename = "ref")]
    git_ref: String,
    after: String,
    #[serde(default)]
    deleted: bool,
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
        Some("ping") => return Ok(Json(json!({ "ok": true, "runs": [] }))),
        Some("push") => {}
        other => return Err(bad_request(format!("unsupported event {other:?}"))),
    }
    let ev: PushEvent = serde_json::from_slice(&body).map_err(|e| bad_request(e.to_string()))?;
    if ev.deleted {
        return Ok(Json(json!({ "ok": true, "runs": [] })));
    }
    let urls: Vec<String> = [
        &ev.repository.clone_url,
        &ev.repository.ssh_url,
        &ev.repository.html_url,
    ]
    .into_iter()
    .flatten()
    .map(|u| normalize_repo_url(u))
    .collect();
    let mut runs = Vec::new();
    for p in s.engine.db.projects()? {
        if urls.contains(&normalize_repo_url(&p.repo_url)) {
            runs.push(
                s.engine
                    .trigger(p.id, &ev.git_ref, Some(&ev.after), "push")?,
            );
        }
    }
    Ok(Json(json!({ "ok": true, "runs": runs })))
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
