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
        crate::reviews::spawn_queue(engine.clone());
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

async fn wait_run(engine: &Engine, repo_id: i64, sha: &str) -> String {
    for _ in 0..200 {
        if let Some(r) = engine.db.run_for_sha(repo_id, sha).unwrap()
            && !matches!(r.status.as_str(), "queued" | "running")
        {
            return r.status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no finished run for {sha}");
}

/// Project `shop` with a hosted repo `api` whose main has a pipeline and review rules.
async fn shop(srv: &Server, review_rules: &str) -> (i64, i64, PathBuf) {
    let (_, p) = srv.api("POST", "/api/projects", Some(serde_json::json!({"name": "shop", "key": "SHOP"}))).await;
    let pid = p["id"].as_i64().unwrap();
    let (_, repo) = srv.api("POST", &format!("/api/projects/{pid}/repos"), Some(serde_json::json!({"name": "api"}))).await;
    let rid = repo["id"].as_i64().unwrap();
    let work = srv.dir.path().join("shop-api");
    ok(srv.dir.path(), &["clone", "-q", &srv.url("shop", "api"), work.to_str().unwrap()]);
    std::fs::write(work.join(".moloko.yml"), format!("review: {review_rules}\njobs:\n  test:\n    steps: [test ! -f broken]\n")).unwrap();
    let sha = commit(&work, "app.txt", "v1\n", "initial");
    ok(&work, &["push", "-q", "origin", "HEAD:main"]);
    assert_eq!(wait_run(&srv.engine, rid, &sha).await, "success");
    (pid, rid, work)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_to_review_to_merge() {
    let srv = Server::start().await;
    let (pid, rid, work) = shop(&srv, "{required_approvals: 1, merge: squash}").await;
    let (st, issue) = srv.api("POST", &format!("/api/projects/{pid}/issues"), Some(serde_json::json!({"title": "Fix the login page", "type": "bug"}))).await;
    assert_eq!(st, 201, "{issue}");
    assert_eq!(issue["key"], "SHOP-1");
    let iid = issue["id"].as_i64().unwrap();

    // "Create branch" on the issue: the first push moves it to in progress.
    let (st, b) = srv.api("POST", &format!("/api/issues/{iid}/branch"), Some(serde_json::json!({"repo_id": rid}))).await;
    assert_eq!(st, 201, "{b}");
    let branch = b["branch"].as_str().unwrap().to_string();
    assert_eq!(branch, "SHOP-1-fix-the-login-page");
    let (_, d) = srv.api("GET", &format!("/api/issues/{iid}"), None).await;
    assert_eq!(d["issue"]["status"], "in_progress");

    ok(&work, &["fetch", "-q", "origin"]);
    ok(&work, &["checkout", "-q", "-b", &branch, &format!("origin/{branch}")]);
    let head = commit(&work, "app.txt", "v2\n", "Fix login (SHOP-1)");
    ok(&work, &["push", "-q", "origin", &branch]);
    assert_eq!(wait_run(&srv.engine, rid, &head).await, "success");

    let (st, review) = srv.api("POST", &format!("/api/projects/{pid}/reviews"), Some(serde_json::json!({"repo_id": rid, "source_branch": branch}))).await;
    assert_eq!(st, 201, "{review}");
    let vid = review["id"].as_i64().unwrap();
    assert_eq!(review["issue_key"], "SHOP-1");
    let (_, d) = srv.api("GET", &format!("/api/issues/{iid}"), None).await;
    assert_eq!(d["issue"]["status"], "in_review");
    assert!(d["refs"].as_array().unwrap().iter().any(|r| r["kind"] == "commit"));

    // Not mergeable without an approval; the author can't approve.
    let (_, rv) = srv.api("GET", &format!("/api/reviews/{vid}"), None).await;
    assert_eq!(rv["status"]["mergeable"], false);
    assert!(rv["diff"].as_str().unwrap().contains("+v2"));
    let (st, _) = srv.api("POST", &format!("/api/reviews/{vid}/approve"), Some(serde_json::json!({"state": "approved"}))).await;
    assert_eq!(st, 400);
    let (st, _) = srv.api("POST", &format!("/api/reviews/{vid}/merge"), None).await;
    assert_eq!(st, 409);

    let bob = srv.engine.db.create_user("bob", Some("password1"), "human", false).unwrap();
    srv.engine.db.set_member(pid, bob, crate::auth::Role::Write).unwrap();
    let (_, bob_token) = srv.engine.db.create_token(bob, "t", None, None).unwrap();
    let (st, _) = srv.api_as(&bob_token, "POST", &format!("/api/reviews/{vid}/comments"), Some(serde_json::json!({"body": "nit", "path": "app.txt", "line": 1}))).await;
    assert_eq!(st, 201);
    let (st, _) = srv.api_as(&bob_token, "POST", &format!("/api/reviews/{vid}/approve"), Some(serde_json::json!({"state": "approved"}))).await;
    assert_eq!(st, 204);
    let (_, rv) = srv.api("GET", &format!("/api/reviews/{vid}"), None).await;
    assert!(rv["status"]["blockers"].as_array().unwrap().iter().any(|b| b.as_str().unwrap().contains("unresolved")), "{}", rv["status"]);
    let cid = rv["comments"][0]["id"].as_i64().unwrap();
    srv.api("POST", &format!("/api/reviews/{vid}/comments/{cid}/resolve"), Some(serde_json::json!({"resolved": true}))).await;
    let (st, m) = srv.api("POST", &format!("/api/reviews/{vid}/merge"), None).await;
    assert_eq!(st, 200, "{m}");

    let local = srv.engine.git.path(rid);
    let main = remote_sha(&local, "refs/heads/main").unwrap();
    assert_eq!(ok(&local, &["log", "-1", "--format=%s", &main]).trim(), "SHOP-1: Fix the login page (!1)");
    let (_, d) = srv.api("GET", &format!("/api/issues/{iid}"), None).await;
    assert_eq!(d["issue"]["status"], "done");
    let (_, rv) = srv.api("GET", &format!("/api/reviews/{vid}"), None).await;
    assert_eq!(rv["review"]["state"], "merged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_tests_merge_result() {
    let srv = Server::start().await;
    let (pid, rid, work) = shop(&srv, "{required_approvals: 0, merge_queue: true}").await;
    ok(&work, &["checkout", "-q", "-b", "feature"]);
    let head = commit(&work, "f.txt", "1", "feature");
    ok(&work, &["push", "-q", "origin", "feature"]);
    assert_eq!(wait_run(&srv.engine, rid, &head).await, "success");
    let (_, review) = srv.api("POST", &format!("/api/projects/{pid}/reviews"), Some(serde_json::json!({"repo_id": rid, "source_branch": "feature"}))).await;
    let vid = review["id"].as_i64().unwrap();
    let (st, m) = srv.api("POST", &format!("/api/reviews/{vid}/merge"), None).await;
    assert_eq!((st, m["result"].as_str()), (200, Some("queued")), "{m}");
    for _ in 0..200 {
        if srv.engine.db.review(vid).unwrap().unwrap().state == "merged" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = srv.engine.db.review(vid).unwrap().unwrap();
    assert_eq!(r.state, "merged");
    let runs = srv.engine.db.runs(Some(pid), 20).unwrap();
    assert!(runs.iter().any(|x| x.trigger == "merge-queue" && x.status == "success"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_agent_implements_assigned_issue() {
    let srv = Server::start().await;
    let (pid, rid, _work) = shop(&srv, "{required_approvals: 0}").await;
    let (st, agent) = srv
        .api("POST", &format!("/api/projects/{pid}/ai-agents"), Some(serde_json::json!({
            "name": "coder",
            "environment": {"kind": "host", "image": ""},
            "command": "test -f \"$MOLOKO_TASK_FILE\" && grep -q 'Add a changelog' \"$MOLOKO_TASK_FILE\" && echo \"done by $MOLOKO_AGENT\" > CHANGELOG.md && echo \"$OPENAI_KEY\" > /dev/null",
            "secrets": ["OPENAI_KEY"]
        })))
        .await;
    assert_eq!(st, 201, "{agent}");
    srv.api("PUT", &format!("/api/projects/{pid}/secrets/OPENAI_KEY"), Some(serde_json::json!({"value": "sk-test"}))).await;
    let (_, issue) = srv.api("POST", &format!("/api/projects/{pid}/issues"), Some(serde_json::json!({"title": "Add a changelog", "assignee": "coder"}))).await;
    let iid = issue["id"].as_i64().unwrap();
    let mut review = None;
    for _ in 0..200 {
        let (_, d) = srv.api("GET", &format!("/api/issues/{iid}"), None).await;
        if d["sessions"][0]["state"] == "success" || d["sessions"][0]["state"] == "failed" {
            review = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let d = review.expect("session finished");
    assert_eq!(d["sessions"][0]["state"], "success", "{d}");
    let r = &d["reviews"][0];
    assert_eq!(r["author"], "coder");
    assert_eq!(r["source_branch"], "SHOP-1-add-a-changelog");
    let local = srv.engine.git.path(rid);
    let content = ok(&local, &["show", "refs/heads/SHOP-1-add-a-changelog:CHANGELOG.md"]);
    assert_eq!(content.trim(), "done by coder");
    // Changes by an AI agent need a person's approval, and agents can't merge.
    let vid = r["id"].as_i64().unwrap();
    let head = r["head_sha"].as_str().unwrap().to_string();
    wait_run(&srv.engine, rid, &head).await;
    let (_, rv) = srv.api("GET", &format!("/api/reviews/{vid}"), None).await;
    assert!(rv["status"]["blockers"].as_array().unwrap().iter().any(|b| b.as_str().unwrap().contains("person")), "{}", rv["status"]);
    assert_eq!(d["issue"]["status"], "in_review");
}
