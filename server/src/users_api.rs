//! Login, users, personal access tokens and project members.

use crate::api::{ApiError, ApiResult, AppState, bad_request, need, need_admin, not_found};
use crate::auth::{Actor, Role, SESSION_COOKIE, cookie};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/me/password", post(change_password))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", delete(delete_token))
        .route("/api/projects/{id}/members", put(set_member))
        .route("/api/projects/{id}/members/{user_id}", delete(remove_member))
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

fn session_cookie(headers: &HeaderMap, token: &str, max_age: i64) -> String {
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        == Some("https");
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    )
}

async fn me(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    match s.engine.db.authenticate(&headers)? {
        Some(a) => Ok((StatusCode::OK, Json(json!({ "user": a.user })))),
        None => Ok((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "login required", "setup_required": s.engine.db.user_count()? == 0 })),
        )),
    }
}

/// Create the first admin. Only works while there are no users.
async fn setup(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(c): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
    if s.engine.db.user_count()? > 0 {
        return Err(ApiError(StatusCode::CONFLICT, "already set up".into()));
    }
    if c.password.len() < 8 {
        return Err(bad_request("password must be at least 8 characters"));
    }
    let id = s
        .engine
        .db
        .create_user(c.username.trim(), Some(&c.password), "human", true)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    // Projects created before auth existed stay reachable: the first admin sees everything.
    let token = s.engine.db.create_session(id)?;
    let user = s.engine.db.user(id)?;
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, session_cookie(&headers, &token, 30 * 86400))],
        Json(json!({ "user": user })),
    ))
}

async fn login(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(c): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
    let user = s
        .engine
        .db
        .check_password(c.username.trim(), &c.password)?
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "wrong username or password".into()))?;
    let token = s.engine.db.create_session(user.id)?;
    Ok((
        [(header::SET_COOKIE, session_cookie(&headers, &token, 30 * 86400))],
        Json(json!({ "user": user })),
    ))
}

async fn logout(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    if let Some(t) = cookie(&headers, SESSION_COOKIE) {
        s.engine.db.delete_session(&t)?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie(&headers, "", 0))],
    ))
}

async fn list_users(State(s): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(s.engine.db.users()?))
}

#[derive(Deserialize)]
struct NewUser {
    username: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
}

async fn create_user(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(u): Json<NewUser>,
) -> ApiResult<impl IntoResponse> {
    need_admin(&actor)?;
    if u.password.len() < 8 {
        return Err(bad_request("password must be at least 8 characters"));
    }
    let id = s
        .engine
        .db
        .create_user(u.username.trim(), Some(&u.password), "human", u.is_admin)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

async fn change_password(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(p): Json<PasswordChange>,
) -> ApiResult<impl IntoResponse> {
    if s.engine.db.check_password(&actor.user.username, &p.current)?.is_none() {
        return Err(ApiError(StatusCode::FORBIDDEN, "current password is wrong".into()));
    }
    if p.new.len() < 8 {
        return Err(bad_request("password must be at least 8 characters"));
    }
    s.engine.db.set_password(actor.user.id, &p.new)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_tokens(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(s.engine.db.tokens(actor.user.id)?))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

async fn create_token(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(t): Json<NewToken>,
) -> ApiResult<impl IntoResponse> {
    if actor.scope.is_some() {
        return Err(ApiError(StatusCode::FORBIDDEN, "scoped tokens can't create tokens".into()));
    }
    let name = t.name.trim();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let (id, token) = s.engine.db.create_token(actor.user.id, name, None, None)?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "token": token }))))
}

async fn delete_token(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    if s.engine.db.delete_token(actor.user.id, id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

#[derive(Deserialize)]
struct MemberBody {
    username: String,
    role: Role,
}

async fn set_member(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(m): Json<MemberBody>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    let user = s
        .engine
        .db
        .user_by_name(m.username.trim())?
        .ok_or_else(|| bad_request("no such user"))?;
    s.engine.db.set_member(id, user.id, m.role)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_member(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, user_id)): Path<(i64, i64)>,
) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Admin)?;
    if s.engine.db.remove_member(id, user_id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}
