//! End-to-end tests: a real HTTP server, the real `git` client, and local "GitHub" repos.

use crate::api::{AppState, router};
use crate::engine::Engine;
use crate::repos::MirrorMode;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

pub struct Server {
    pub dir: tempfile::TempDir,
    pub engine: Arc<Engine>,
    pub base: String,
    pub token: String,
    pub state: AppState,
}

impl Server {
    pub async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(&dir.path().join("data"), 4, "localhost:0").unwrap();
        let uid = engine.db.create_user("admin", Some("password1"), "human", true).unwrap();
        let (_, token) = engine.db.create_token(uid, "test", None, None).unwrap();
        let state = AppState { engine: engine.clone(), webhook_secret: None, wg: None };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { dir, engine, base, token, state }
    }

    /// Clone URL with credentials.
    pub fn url(&self, project: &str, repo: &str) -> String {
        format!("{}/git/{project}/{repo}.git", self.base.replace("http://", &format!("http://admin:{}@", self.token)))
    }

    pub async fn api(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.api_as(&self.token.clone(), method, path, body).await
    }

    pub async fn api_as(&self, token: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"));
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = router(self.state.clone()).oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

pub fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

pub fn ok(dir: &Path, args: &[&str]) -> String {
    let (good, text) = git(dir, args);
    assert!(good, "git {args:?} failed: {text}");
    text
}

pub fn commit(dir: &Path, file: &str, content: &str, msg: &str) -> String {
    std::fs::write(dir.join(file), content).unwrap();
    ok(dir, &["add", "."]);
    ok(dir, &["commit", "-q", "-m", msg]);
    ok(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// A bare repo standing in for GitHub, seeded with one commit on main.
pub fn upstream(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let bare = root.join(format!("{name}-gh.git"));
    ok(root, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
    let work = root.join(format!("{name}-gh-work"));
    ok(root, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    commit(&work, "README.md", "hello\n", "initial");
    ok(&work, &["push", "-q", "origin", "main"]);
    (bare, work)
}

pub fn remote_sha(bare: &Path, r: &str) -> Option<String> {
    let (good, out) = git(bare, &["rev-parse", "--verify", "-q", r]);
    good.then(|| out.trim().to_string())
}

pub async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not met in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_repo_push_clone_and_protection() {
    let srv = Server::start().await;
    let (st, p) = srv.api("POST", "/api/projects", Some(serde_json::json!({"name": "shop"}))).await;
    assert_eq!(st, 201, "{p}");
    let pid = p["id"].as_i64().unwrap();
    let (st, repo) = srv.api("POST", &format!("/api/projects/{pid}/repos"), Some(serde_json::json!({"name": "api"}))).await;
    assert_eq!(st, 201, "{repo}");
    let rid = repo["id"].as_i64().unwrap();

    let work = srv.dir.path().join("work");
    ok(srv.dir.path(), &["clone", "-q", &srv.url("shop", "api"), work.to_str().unwrap()]);
    let first = commit(&work, "a.txt", "1", "first");
    ok(&work, &["push", "-q", "origin", "HEAD:main"]);
    commit(&work, "a.txt", "2", "second");
    ok(&work, &["push", "-q", "origin", "HEAD:main"]);

    // A fresh clone sees both commits.
    let clone2 = srv.dir.path().join("clone2");
    ok(srv.dir.path(), &["clone", "-q", &srv.url("shop", "api"), clone2.to_str().unwrap()]);
    assert_eq!(ok(&clone2, &["rev-list", "--count", "HEAD"]).trim(), "2");

    // Force-pushing the protected default branch is refused; feature branches are fine.
    ok(&work, &["reset", "-q", "--hard", &first]);
    commit(&work, "b.txt", "x", "rewrite");
    let (good, out) = git(&work, &["push", "-q", "-f", "origin", "HEAD:main"]);
    assert!(!good && out.contains("protected"), "{out}");
    let (good, out) = git(&work, &["push", "origin", "HEAD:refs/heads/MOL-1-feature"]);
    assert!(good, "{out}");
    assert!(out.contains("Open a review for MOL-1-feature"), "{out}");
    ok(&work, &["push", "-q", "origin", "--delete", "MOL-1-feature"]);

    // Browsing.
    let (st, tree) = srv.api("GET", &format!("/api/repos/{rid}/tree"), None).await;
    assert_eq!(st, 200);
    assert_eq!(tree["entries"][0]["name"], "a.txt");
    let (_, blob) = srv.api("GET", &format!("/api/repos/{rid}/blob?path=a.txt"), None).await;
    assert_eq!(blob["content"], "2");
    let (_, commits) = srv.api("GET", &format!("/api/repos/{rid}/commits"), None).await;
    assert_eq!(commits.as_array().unwrap().len(), 2);
    let sha = commits[0]["commit"]["sha"].as_str().unwrap();
    let (_, c) = srv.api("GET", &format!("/api/repos/{rid}/commits/{sha}"), None).await;
    assert!(c["diff"].as_str().unwrap().contains("-1"));

    // Wrong credentials and other users' projects.
    let anon = format!("{}/git/shop/api.git", srv.base);
    let (good, _) = git(srv.dir.path(), &["ls-remote", &anon]);
    assert!(!good);
    let other = srv.engine.db.create_user("eve", Some("password1"), "human", false).unwrap();
    let (_, eve) = srv.engine.db.create_token(other, "t", None, None).unwrap();
    let (st, _) = srv.api_as(&eve, "GET", &format!("/api/repos/{rid}/tree"), None).await;
    assert_eq!(st, 404);
    let (st, list) = srv.api_as(&eve, "GET", "/api/projects", None).await;
    assert_eq!(st, 200);
    assert_eq!(list.as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_mirror_follows_upstream_and_refuses_pushes() {
    let srv = Server::start().await;
    let (gh, gh_work) = upstream(srv.dir.path(), "pull");
    let pid = srv.engine.db.create_project("lib", gh.to_str().unwrap()).unwrap();
    let repo = srv.engine.db.repos(pid).unwrap().remove(0);
    assert_eq!(repo.mirror_mode, MirrorMode::Pull);
    crate::mirror::sync(&srv.engine, &repo).await.unwrap();
    let path = srv.engine.git.path(repo.id);
    assert_eq!(remote_sha(&path, "refs/heads/main"), remote_sha(&gh, "refs/heads/main"));

    commit(&gh_work, "x", "1", "upstream change");
    ok(&gh_work, &["push", "-q", "origin", "main"]);
    let updates = crate::mirror::sync(&srv.engine, &repo).await.unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(remote_sha(&path, "refs/heads/main"), remote_sha(&gh, "refs/heads/main"));

    let work = srv.dir.path().join("w");
    ok(srv.dir.path(), &["clone", "-q", &srv.url("lib", &repo.name), work.to_str().unwrap()]);
    commit(&work, "y", "1", "local");
    let (good, out) = git(&work, &["push", "origin", "HEAD:main"]);
    assert!(!good && out.contains("push there instead"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_mirror_copies_changes_upstream() {
    let srv = Server::start().await;
    let (gh, _) = upstream(srv.dir.path(), "push");
    let (_, p) = srv.api("POST", "/api/projects", Some(serde_json::json!({"name": "web"}))).await;
    let pid = p["id"].as_i64().unwrap();
    let (st, repo) = srv
        .api("POST", &format!("/api/projects/{pid}/repos"), Some(serde_json::json!({"name": "web", "upstream_url": gh.to_str().unwrap()})))
        .await;
    assert_eq!(st, 201);
    assert_eq!(repo["mirror_mode"], "push");
    let rid = repo["id"].as_i64().unwrap();
    let local = srv.engine.git.path(rid);
    eventually(|| remote_sha(&local, "refs/heads/main").is_some()).await;

    let work = srv.dir.path().join("w");
    ok(srv.dir.path(), &["clone", "-q", &srv.url("web", "web"), work.to_str().unwrap()]);
    let sha = commit(&work, "z", "1", "from moloko");
    ok(&work, &["push", "-q", "origin", "HEAD:main"]);
    eventually(|| remote_sha(&gh, "refs/heads/main").as_deref() == Some(sha.as_str())).await;
    let (_, refs) = srv.api("GET", &format!("/api/repos/{rid}/mirror"), None).await;
    let main = refs.as_array().unwrap().iter().find(|r| r["ref"] == "refs/heads/main").unwrap().clone();
    assert_eq!(main["state"], "in_sync");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twoway_writes_through_and_detects_divergence() {
    let srv = Server::start().await;
    let (gh, gh_work) = upstream(srv.dir.path(), "two");
    let pid = srv.engine.db.create_project("two", "").unwrap();
    let rid = srv.engine.db.create_repo(pid, "two", Some(gh.to_str().unwrap()), MirrorMode::Twoway).unwrap();
    let repo = srv.engine.db.repo(rid).unwrap().unwrap();
    crate::code::import(&srv.engine, &repo).await.unwrap();
    let local = srv.engine.git.path(rid);

    // A push to moloko lands upstream before moloko accepts it.
    let work = srv.dir.path().join("w");
    ok(srv.dir.path(), &["clone", "-q", &srv.url("two", "two"), work.to_str().unwrap()]);
    let a = commit(&work, "a", "1", "via moloko");
    ok(&work, &["push", "-q", "origin", "HEAD:main"]);
    assert_eq!(remote_sha(&gh, "refs/heads/main").as_deref(), Some(a.as_str()));

    // Someone pushes to GitHub directly; moloko fast-forwards.
    ok(&gh_work, &["pull", "-q", "origin", "main"]);
    let b = commit(&gh_work, "b", "1", "via github");
    ok(&gh_work, &["push", "-q", "origin", "main"]);
    // Before moloko has fetched, a push based on the old head is rejected, not merged blindly.
    commit(&work, "c", "1", "stale base");
    let (good, out) = git(&work, &["push", "origin", "HEAD:main"]);
    assert!(!good && out.contains("fetch first"), "{out}");
    crate::mirror::sync(&srv.engine, &repo).await.unwrap();
    assert_eq!(remote_sha(&local, "refs/heads/main").as_deref(), Some(b.as_str()));

    // Upstream unreachable with `queue`: accepted locally, then both sides move: diverged.
    srv.engine.db.update_repo(rid, crate::repos::RepoSettings { on_unreachable: Some("queue".into()), ..Default::default() }).unwrap();
    let hidden = srv.dir.path().join("hidden.git");
    std::fs::rename(&gh, &hidden).unwrap();
    ok(&work, &["fetch", "-q", "origin"]);
    ok(&work, &["reset", "-q", "--hard", "origin/main"]);
    let mine = commit(&work, "d", "1", "while github was down");
    ok(&work, &["push", "-q", "origin", "HEAD:main"]);
    std::fs::rename(&hidden, &gh).unwrap();
    commit(&gh_work, "e", "1", "github meanwhile");
    ok(&gh_work, &["push", "-q", "origin", "main"]);
    crate::mirror::sync(&srv.engine, &repo).await.unwrap();
    let refs = srv.engine.db.mirror_refs(rid).unwrap();
    let main = refs.iter().find(|r| r.name == "refs/heads/main").unwrap();
    assert_eq!(main.state, "diverged");
    assert_eq!(remote_sha(&local, "refs/heads/main").as_deref(), Some(mine.as_str()));

    crate::mirror::resolve_diverged(&srv.engine, &repo, "refs/heads/main", "moloko").await.unwrap();
    assert_eq!(remote_sha(&gh, "refs/heads/main").as_deref(), Some(mine.as_str()));
    assert!(remote_sha(&local, "refs/moloko/diverged/upstream/heads/main").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_flow() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(&dir.path().join("data"), 1, "localhost:0").unwrap();
    let state = AppState { engine: engine.clone(), webhook_secret: None, wg: None };
    let call = |req: Request<Body>| {
        let app = router(state.clone());
        async move {
            let res = app.oneshot(req).await.unwrap();
            let status = res.status().as_u16();
            let cookie = res.headers().get("set-cookie").map(|v| v.to_str().unwrap().to_string());
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (status, cookie, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let json_req = |m: &str, p: &str, b: Value| {
        Request::builder().method(m).uri(p).header("content-type", "application/json").body(Body::from(b.to_string())).unwrap()
    };
    let (st, _, me) = call(Request::get("/api/auth/me").body(Body::empty()).unwrap()).await;
    assert_eq!((st, me["setup_required"].as_bool()), (401, Some(true)));
    let (st, _, _) = call(Request::get("/api/projects").body(Body::empty()).unwrap()).await;
    assert_eq!(st, 401);
    let (st, cookie, _) = call(json_req("POST", "/api/auth/setup", serde_json::json!({"username": "dani", "password": "longenough"}))).await;
    assert_eq!(st, 201);
    let session = cookie.unwrap().split(';').next().unwrap().to_string();
    let (st, _, _) = call(json_req("POST", "/api/auth/setup", serde_json::json!({"username": "x", "password": "longenough"}))).await;
    assert_eq!(st, 409);
    let (st, _, _) = call(Request::get("/api/projects").header("cookie", &session).body(Body::empty()).unwrap()).await;
    assert_eq!(st, 200);
    let (st, _, _) = call(json_req("POST", "/api/auth/login", serde_json::json!({"username": "dani", "password": "wrong"}))).await;
    assert_eq!(st, 401);
    let (st, cookie, _) = call(json_req("POST", "/api/auth/login", serde_json::json!({"username": "dani", "password": "longenough"}))).await;
    assert_eq!(st, 200);
    assert!(cookie.unwrap().contains("HttpOnly"));
}
