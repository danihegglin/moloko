//! Reviews: a branch heading into a target, with its diff, line comments, approvals, required
//! checks, code owners, a merge queue, and change sets (reviews in several repos for one issue
//! that land together).

use crate::api::{ApiError, ApiResult, AppState, bad_request, need, not_found};
use crate::auth::{Actor, Role, User};
use crate::config::{PIPELINE_FILE, Pipeline, ReviewRules};
use crate::db::{Db, Status, now_ms};
use crate::engine::{Engine, RunCtx};
use crate::git::{self, git};
use crate::repos::{RefUpdate, Repo, Source};
use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS reviews (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    source_branch TEXT NOT NULL,
    target_branch TEXT NOT NULL,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    merge_sha TEXT,
    issue_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
    queue_state TEXT,
    queue_sha TEXT,
    queue_base TEXT,
    queue_run_id INTEGER,
    queued_at INTEGER,
    merged_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    merged_at INTEGER,
    UNIQUE (project_id, number)
);
CREATE TABLE IF NOT EXISTS review_comments (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    parent_id INTEGER REFERENCES review_comments(id) ON DELETE CASCADE,
    path TEXT,
    line INTEGER,
    side TEXT,
    sha TEXT,
    body TEXT NOT NULL,
    resolved INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS review_approvals (
    review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    state TEXT NOT NULL,
    sha TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (review_id, user_id)
);
CREATE TABLE IF NOT EXISTS review_events (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub id: i64,
    pub project_id: i64,
    pub repo_id: i64,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub source_branch: String,
    pub target_branch: String,
    pub author: Option<String>,
    pub author_id: Option<i64>,
    pub author_kind: Option<String>,
    /// `draft`, `open`, `merged` or `closed`.
    pub state: String,
    pub head_sha: String,
    pub merge_sha: Option<String>,
    pub issue_id: Option<i64>,
    pub issue_key: Option<String>,
    /// `queued`, `testing` or `failed` while in the merge queue.
    pub queue_state: Option<String>,
    pub queue_run_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub merged_at: Option<i64>,
}

const COLS: &str = "v.id, v.project_id, v.repo_id, r.name, v.number, v.title, v.body, v.source_branch, v.target_branch,
    u.username, v.author_id, u.kind, v.state, v.head_sha, v.merge_sha, v.issue_id,
    CASE WHEN i.id IS NULL THEN NULL ELSE p.key || '-' || i.number END, v.queue_state, v.queue_run_id,
    v.created_at, v.updated_at, v.merged_at";
const FROM: &str = "reviews v JOIN repos r ON r.id = v.repo_id JOIN projects p ON p.id = v.project_id
    LEFT JOIN users u ON u.id = v.author_id LEFT JOIN issues i ON i.id = v.issue_id";

fn map(r: &rusqlite::Row) -> rusqlite::Result<Review> {
    Ok(Review {
        id: r.get(0)?,
        project_id: r.get(1)?,
        repo_id: r.get(2)?,
        repo: r.get(3)?,
        number: r.get(4)?,
        title: r.get(5)?,
        body: r.get(6)?,
        source_branch: r.get(7)?,
        target_branch: r.get(8)?,
        author: r.get(9)?,
        author_id: r.get(10)?,
        author_kind: r.get(11)?,
        state: r.get(12)?,
        head_sha: r.get(13)?,
        merge_sha: r.get(14)?,
        issue_id: r.get(15)?,
        issue_key: r.get(16)?,
        queue_state: r.get(17)?,
        queue_run_id: r.get(18)?,
        created_at: r.get(19)?,
        updated_at: r.get(20)?,
        merged_at: r.get(21)?,
    })
}

impl Db {
    pub fn review(&self, id: i64) -> Result<Option<Review>> {
        let c = self.conn();
        Ok(c.query_row(&format!("SELECT {COLS} FROM {FROM} WHERE v.id = ?1"), [id], map).optional()?)
    }

    fn reviews_where(&self, cond: &str, args: impl rusqlite::Params) -> Result<Vec<Review>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM {FROM} WHERE {cond} ORDER BY v.id DESC"))?;
        let rows = st.query_map(args, map)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn reviews(&self, project_id: i64, state: &str) -> Result<Vec<Review>> {
        match state {
            "all" => self.reviews_where("v.project_id = ?1", [project_id]),
            "open" => self.reviews_where("v.project_id = ?1 AND v.state IN ('open', 'draft')", [project_id]),
            s => self.reviews_where("v.project_id = ?1 AND v.state = ?2", params![project_id, s]),
        }
    }

    pub fn active_reviews_for_branch(&self, repo_id: i64, branch: &str) -> Result<Vec<Review>> {
        self.reviews_where("v.repo_id = ?1 AND v.source_branch = ?2 AND v.state IN ('open', 'draft')", params![repo_id, branch])
    }

    fn add_review_event(&self, id: i64, actor: Option<i64>, kind: &str, body: &str) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT INTO review_events (review_id, actor_id, kind, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, actor, kind, body, now_ms()],
        )?;
        c.execute("UPDATE reviews SET updated_at = ?2 WHERE id = ?1", params![id, now_ms()])?;
        Ok(())
    }

    fn review_events(&self, id: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT e.kind, e.body, u.username, e.created_at FROM review_events e LEFT JOIN users u ON u.id = e.actor_id WHERE e.review_id = ?1 ORDER BY e.id",
        )?;
        let rows = st.query_map([id], |r| {
            Ok(json!({ "kind": r.get::<_, String>(0)?, "body": r.get::<_, String>(1)?, "actor": r.get::<_, Option<String>>(2)?, "created_at": r.get::<_, i64>(3)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn review_comments(&self, id: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT c.id, c.parent_id, u.username, u.kind, c.path, c.line, c.side, c.sha, c.body, c.resolved, c.created_at
             FROM review_comments c LEFT JOIN users u ON u.id = c.author_id WHERE c.review_id = ?1 ORDER BY c.id",
        )?;
        let rows = st.query_map([id], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "parent_id": r.get::<_, Option<i64>>(1)?, "author": r.get::<_, Option<String>>(2)?,
                "author_kind": r.get::<_, Option<String>>(3)?, "path": r.get::<_, Option<String>>(4)?, "line": r.get::<_, Option<i64>>(5)?,
                "side": r.get::<_, Option<String>>(6)?, "sha": r.get::<_, Option<String>>(7)?, "body": r.get::<_, String>(8)?,
                "resolved": r.get::<_, i64>(9)? != 0, "created_at": r.get::<_, i64>(10)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (username, kind, state, sha) per reviewer.
    fn approvals(&self, id: i64) -> Result<Vec<(String, String, String, String, i64)>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT u.username, u.kind, a.state, a.sha, a.user_id FROM review_approvals a JOIN users u ON u.id = a.user_id WHERE a.review_id = ?1",
        )?;
        let rows = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_approval(&self, review_id: i64, user_id: i64, state: &str, sha: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO review_approvals (review_id, user_id, state, sha, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (review_id, user_id) DO UPDATE SET state = excluded.state, sha = excluded.sha, created_at = excluded.created_at",
            params![review_id, user_id, state, sha, now_ms()],
        )?;
        Ok(())
    }

    pub fn add_review_comment(&self, review_id: i64, author: i64, body: &str, anchor: Option<(&str, i64, &str, &str)>, parent: Option<i64>) -> Result<i64> {
        let c = self.conn();
        let (path, line, side, sha) = match anchor {
            Some((p, l, s, h)) => (Some(p), Some(l), Some(s), Some(h)),
            None => (None, None, None, None),
        };
        c.execute(
            "INSERT INTO review_comments (review_id, author_id, parent_id, path, line, side, sha, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![review_id, author, parent, path, line, side, sha, body, now_ms()],
        )?;
        let id = c.last_insert_rowid();
        c.execute("UPDATE reviews SET updated_at = ?2 WHERE id = ?1", params![review_id, now_ms()])?;
        Ok(id)
    }
}

pub fn for_issue(db: &Db, issue_id: i64) -> Result<Vec<Review>> {
    db.reviews_where("v.issue_id = ?1", [issue_id])
}

/// Glob match for CODEOWNERS patterns: `*` within a path segment, `**` across segments,
/// a trailing `/` matches a directory and everything in it, a leading `/` anchors at the root.
pub fn owner_pattern_matches(pattern: &str, path: &str) -> bool {
    fn glob(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) if p.get(1) == Some(&b'*') => {
                let rest = if p.get(2) == Some(&b'/') { &p[3..] } else { &p[2..] };
                (0..=s.len()).any(|i| glob(rest, &s[i..]))
            }
            (Some(b'*'), _) => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| glob(&p[1..], &s[i..])),
            (Some(a), Some(b)) if a == b => glob(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let anchored = pattern.starts_with('/');
    let mut pat = pattern.trim_start_matches('/').to_string();
    if pat.ends_with('/') {
        pat.push_str("**");
    }
    if !anchored && !pat.contains('/') {
        pat = format!("**/{pat}");
    }
    glob(pat.as_bytes(), path.as_bytes()) || glob(format!("{pat}/**").as_bytes(), path.as_bytes())
}

/// Owners of `path` per CODEOWNERS (last matching line wins).
pub fn owners_of(codeowners: &str, path: &str) -> Vec<String> {
    let mut owners = Vec::new();
    for line in codeowners.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        let Some(pattern) = parts.next() else { continue };
        if owner_pattern_matches(pattern, path) {
            owners = parts.map(|o| o.trim_start_matches('@').to_string()).collect();
        }
    }
    owners
}

#[derive(Debug, Serialize)]
pub struct ReviewStatus {
    pub rules: ReviewRules,
    pub checks: Value,
    pub approvals: Vec<Value>,
    pub blockers: Vec<String>,
    pub mergeable: bool,
    pub behind: bool,
    pub conflicts: bool,
}

/// Rules from `.moloko.yml` at the target branch, or defaults.
async fn rules_at(engine: &Engine, path: &std::path::Path, sha: &str) -> ReviewRules {
    match engine.git.read_file(path, sha, PIPELINE_FILE).await {
        Ok(src) => Pipeline::parse(&src).map(|p| p.review).unwrap_or_default(),
        Err(_) => ReviewRules::default(),
    }
}

pub async fn status(engine: &Arc<Engine>, r: &Review) -> Result<ReviewStatus> {
    let path = engine.git.path(r.repo_id);
    let target = engine.git.resolve(&path, &format!("refs/heads/{}", r.target_branch)).await?;
    let rules = rules_at(engine, &path, &target).await;
    let mut blockers = Vec::new();
    if engine.db.repo(r.repo_id)?.is_some_and(|x| x.mirror_mode == crate::repos::MirrorMode::Pull) {
        blockers.push("this repo mirrors upstream; merge there".to_string());
    }
    if r.state == "draft" {
        blockers.push("the review is a draft".to_string());
    }

    // Checks: a run for the head commit, with the required jobs green.
    let has_pipeline = engine.git.read_file(&path, &r.head_sha, PIPELINE_FILE).await.is_ok();
    let checks = if !has_pipeline {
        json!({ "state": "none" })
    } else {
        match engine.db.run_for_sha(r.repo_id, &r.head_sha)? {
            None => {
                blockers.push("no run for the latest commit yet".into());
                json!({ "state": "missing" })
            }
            Some(run) => {
                let jobs = engine.db.jobs(run.id)?;
                let required: Vec<_> = jobs
                    .iter()
                    .filter(|j| rules.required_jobs.is_empty() || rules.required_jobs.iter().any(|n| j.name == *n || j.name.starts_with(&format!("{n}["))))
                    .collect();
                let passed = !required.is_empty() && required.iter().all(|j| j.status == "success");
                let finished = !matches!(run.status.as_str(), "queued" | "running");
                if !passed {
                    blockers.push(if finished { "required checks failed".into() } else { "checks are still running".into() });
                }
                json!({ "state": if passed { "success" } else if finished { "failed" } else { "running" }, "run": run, "required": required.iter().map(|j| json!({"name": j.name, "status": j.status})).collect::<Vec<_>>() })
            }
        }
    };

    // Approvals for the current head only; pushing new commits asks for a fresh look.
    let all = engine.db.approvals(r.id)?;
    let current: Vec<_> = all.iter().filter(|a| a.3 == r.head_sha && Some(a.4) != r.author_id).collect();
    let approved: Vec<_> = current.iter().filter(|a| a.2 == "approved").collect();
    if current.iter().any(|a| a.2 == "changes_requested") {
        blockers.push("changes were requested".into());
    }
    if (approved.len() as u32) < rules.required_approvals {
        blockers.push(format!("{} of {} approvals", approved.len(), rules.required_approvals));
    }
    if rules.human_approval_for_ai_changes && r.author_kind.as_deref() == Some("bot") && !approved.iter().any(|a| a.1 == "human") {
        blockers.push("changes by an AI agent need a person's approval".into());
    }
    if rules.ai_review == "required" {
        let reviewer = rules.ai_reviewer.as_deref().unwrap_or("");
        if !approved.iter().any(|a| a.0 == reviewer) {
            blockers.push(format!("waiting for the AI review by {reviewer}"));
        }
    }

    let base = git(&["merge-base", &target, &r.head_sha], Some(&path)).await.unwrap_or_default();
    if rules.code_owners
        && let Ok(owners) = engine.git.read_file(&path, &target, ".moloko/CODEOWNERS").await
    {
        let (files, _) = crate::code::diff(&path, &base, &r.head_sha).await?;
        let approvers: Vec<&str> = approved.iter().map(|a| a.0.as_str()).collect();
        let mut missing: Vec<String> = Vec::new();
        for f in files {
            let o = owners_of(&owners, &f.path);
            if !o.is_empty() && !o.iter().any(|x| approvers.contains(&x.as_str())) {
                let who = o.join(" or ");
                if !missing.contains(&who) {
                    missing.push(who);
                }
            }
        }
        for m in missing {
            blockers.push(format!("needs a code owner's approval: {m}"));
        }
    }

    let unresolved = engine.db.review_comments(r.id)?.iter().filter(|c| c["parent_id"].is_null() && !c["resolved"].as_bool().unwrap_or(false)).count();
    if unresolved > 0 {
        blockers.push(format!("{unresolved} unresolved thread(s)"));
    }
    let behind = base != target;
    let conflicts = behind && merge_tree(&path, &target, &r.head_sha).await.is_err();
    if conflicts {
        blockers.push("the branch has conflicts with the target".into());
    }
    let approvals = all
        .iter()
        .map(|a| json!({ "user": a.0, "kind": a.1, "state": a.2, "stale": a.3 != r.head_sha }))
        .collect();
    Ok(ReviewStatus { mergeable: blockers.is_empty() && r.state == "open", rules, checks, approvals, blockers, behind, conflicts })
}

/// Check a merge for conflicts without a worktree. Returns the merged tree.
async fn merge_tree(path: &std::path::Path, a: &str, b: &str) -> Result<String> {
    let out = git::git_output(&["merge-tree", "--write-tree", a, b], Some(path), &[], None).await?;
    if !out.status.success() {
        bail!("conflicts");
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string())
}

/// Build the commit that merging `r` onto `target` would produce, per the merge method.
async fn build_merge(engine: &Engine, r: &Review, target: &str, method: &str, by: &User) -> Result<String> {
    let path = engine.git.path(r.repo_id);
    let email = format!("{}@moloko", by.username);
    let env = git::identity_env(&by.display_name, &email);
    let key = r.issue_key.as_deref().map(|k| format!("\n\nCloses {k}")).unwrap_or_default();
    match method {
        "squash" => {
            let tree = merge_tree(&path, target, &r.head_sha).await.context("the branch has conflicts")?;
            let msg = format!("{} (!{}){key}\n\n{}", r.title, r.number, r.body);
            git::git_env(&["commit-tree", &tree, "-p", target, "-m", msg.trim_end()], Some(&path), &env).await
        }
        "rebase" => {
            // Replay each commit with merge-tree, so no worktree is needed.
            let base = git(&["merge-base", target, &r.head_sha], Some(&path)).await?;
            let list = git(&["rev-list", "--reverse", "--no-merges", &format!("{base}..{}", r.head_sha)], Some(&path)).await?;
            let mut tip = target.to_string();
            for c in list.lines() {
                let parent = format!("{c}^");
                let out = git::git_output(&["merge-tree", "--write-tree", "--merge-base", &parent, &tip, c], Some(&path), &[], None).await?;
                if !out.status.success() {
                    bail!("rebase conflicts at {}", &c[..7]);
                }
                let tree = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string();
                let msg = git(&["log", "-1", "--format=%B", c], Some(&path)).await?;
                let mut env = env.clone();
                // Keep the original author; moloko is the committer.
                let author = git(&["log", "-1", "--format=%an%x00%ae%x00%ad", "--date=raw", c], Some(&path)).await?;
                let a: Vec<&str> = author.split('\0').collect();
                if a.len() == 3 {
                    env.retain(|(k, _)| !k.starts_with("GIT_AUTHOR"));
                    env.push(("GIT_AUTHOR_NAME".into(), a[0].into()));
                    env.push(("GIT_AUTHOR_EMAIL".into(), a[1].into()));
                    env.push(("GIT_AUTHOR_DATE".into(), a[2].into()));
                }
                tip = git::git_env(&["commit-tree", &tree, "-p", &tip, "-m", &msg], Some(&path), &env).await?;
            }
            Ok(tip)
        }
        _ => {
            let tree = merge_tree(&path, target, &r.head_sha).await.context("the branch has conflicts")?;
            let msg = format!("Merge review !{}: {}{key}", r.number, r.title);
            git::git_env(&["commit-tree", &tree, "-p", target, "-p", &r.head_sha, "-m", &msg], Some(&path), &env).await
        }
    }
}

/// Move the target branch to `new` and record the merge.
async fn land(engine: &Arc<Engine>, r: &Review, old: &str, new: &str, by: Option<i64>) -> Result<()> {
    let repo = engine.db.repo(r.repo_id)?.ok_or_else(|| anyhow!("repo gone"))?;
    let res = engine
        .apply_ref_updates(&repo, None, &[RefUpdate { name: format!("refs/heads/{}", r.target_branch), old: old.into(), new: new.into() }], Source::Internal, true)
        .await?;
    if let Some(Err(e)) = res.into_iter().next() {
        bail!("{e}");
    }
    let now = now_ms();
    engine.db.conn().execute(
        "UPDATE reviews SET state = 'merged', merge_sha = ?2, merged_at = ?3, merged_by = ?4, queue_state = NULL, updated_at = ?3 WHERE id = ?1",
        params![r.id, new, now, by],
    )?;
    engine.db.add_review_event(r.id, by, "merged", &format!("Merged into {} as {}", r.target_branch, &new[..7]))?;
    if let Some(iid) = r.issue_id
        && let Some(issue) = engine.db.issue(iid)?
    {
        let all = for_issue(&engine.db, iid)?;
        // A change set closes the issue once every review in it has merged.
        if all.iter().all(|x| x.state == "merged" || x.id == r.id || x.state == "closed") {
            crate::issues::mark_merged(&engine.db, &issue, &repo, new, &format!("Review !{} merged", r.number))?;
        } else {
            engine.db.set_issue_ref(iid, "merge", Some(repo.id), &format!("{}:{new}", repo.name), Some(new), None)?;
        }
    }
    Ok(())
}

/// Merge now, or enter the merge queue. For a change set, all reviews must be ready.
pub async fn merge(engine: &Arc<Engine>, r: &Review, by: &User) -> Result<String> {
    let set: Vec<Review> = match r.issue_id {
        Some(iid) => for_issue(&engine.db, iid)?.into_iter().filter(|x| x.state == "open" || x.state == "draft").collect(),
        None => vec![r.clone()],
    };
    let mut prepared = Vec::new();
    for x in &set {
        let st = status(engine, x).await?;
        if !st.mergeable {
            bail!("!{} ({}) can't merge yet: {}", x.number, x.repo, st.blockers.join("; "));
        }
        prepared.push((x, st.rules));
    }
    if set.len() == 1 && prepared[0].1.merge_queue {
        engine.db.conn().execute(
            "UPDATE reviews SET queue_state = 'queued', queued_at = ?2 WHERE id = ?1",
            params![r.id, now_ms()],
        )?;
        engine.db.add_review_event(r.id, Some(by.id), "queued", "Added to the merge queue")?;
        engine.queue_notify.notify_one();
        return Ok("queued".into());
    }
    // Build every merge first, so a conflict in one repo stops the whole set before anything lands.
    let mut built = Vec::new();
    for (x, rules) in &prepared {
        let path = engine.git.path(x.repo_id);
        let target = engine.git.resolve(&path, &format!("refs/heads/{}", x.target_branch)).await?;
        let new = build_merge(engine, x, &target, &rules.merge, by).await?;
        built.push((*x, target, new));
    }
    for (x, old, new) in built {
        land(engine, x, &old, &new, Some(by.id)).await?;
    }
    Ok("merged".into())
}

/// One step of the merge queue for every repo: test the next queued review on top of the
/// latest target, and land it when the run passes.
async fn queue_tick(engine: &Arc<Engine>) -> Result<()> {
    let queued: Vec<Review> = engine.db.reviews_where("v.queue_state IN ('queued', 'testing') AND v.state = 'open'", [])?;
    let mut busy_repos = std::collections::HashSet::new();
    for r in queued.iter().filter(|r| r.queue_state.as_deref() == Some("testing")) {
        busy_repos.insert((r.repo_id, r.target_branch.clone()));
    }
    let mut next: Vec<&Review> = queued.iter().filter(|r| r.queue_state.as_deref() == Some("queued")).collect();
    next.sort_by_key(|r| r.id);
    for r in next {
        if !busy_repos.insert((r.repo_id, r.target_branch.clone())) {
            continue;
        }
        let path = engine.git.path(r.repo_id);
        let target = engine.git.resolve(&path, &format!("refs/heads/{}", r.target_branch)).await?;
        let rules = rules_at(engine, &path, &target).await;
        let by = match r.author_id { Some(a) => engine.db.user(a)?, None => None };
        let by = by.ok_or_else(|| anyhow!("review author is gone"))?;
        let sha = match build_merge(engine, r, &target, &rules.merge, &by).await {
            Ok(s) => s,
            Err(e) => {
                fail_queue(engine, r, &format!("couldn't build the merge: {e:#}"))?;
                continue;
            }
        };
        let qref = format!("refs/moloko/queue/{}", r.id);
        git(&["update-ref", &qref, &sha], Some(&path)).await?;
        if engine.git.read_file(&path, &sha, PIPELINE_FILE).await.is_err() {
            land(engine, r, &target, &sha, r.author_id).await?;
            continue;
        }
        let run = engine.trigger(r.repo_id, &qref, Some(&sha), "merge-queue")?;
        engine.db.conn().execute(
            "UPDATE reviews SET queue_state = 'testing', queue_sha = ?2, queue_base = ?3, queue_run_id = ?4 WHERE id = ?1",
            params![r.id, sha, target, run],
        )?;
        engine.db.add_review_event(r.id, None, "queue", &format!("Testing the merge result in run #{run}"))?;
    }
    Ok(())
}

fn fail_queue(engine: &Engine, r: &Review, why: &str) -> Result<()> {
    engine.db.conn().execute("UPDATE reviews SET queue_state = 'failed' WHERE id = ?1", [r.id])?;
    engine.db.add_review_event(r.id, None, "queue_failed", why)
}

pub fn spawn_queue(engine: Arc<Engine>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = engine.queue_notify.notified() => {}
            }
            if let Err(e) = queue_tick(&engine).await {
                tracing::warn!("merge queue: {e:#}");
            }
        }
    });
}

pub async fn on_ref_update(engine: &Arc<Engine>, repo: &Repo, u: &RefUpdate) -> Result<()> {
    let Some(branch) = u.branch() else { return Ok(()) };
    for r in engine.db.active_reviews_for_branch(repo.id, branch)? {
        if u.is_delete() {
            engine.db.conn().execute("UPDATE reviews SET state = 'closed', updated_at = ?2 WHERE id = ?1", params![r.id, now_ms()])?;
            engine.db.add_review_event(r.id, None, "closed", "Closed because the branch was deleted")?;
            continue;
        }
        engine.db.conn().execute(
            "UPDATE reviews SET head_sha = ?2, queue_state = NULL, updated_at = ?3 WHERE id = ?1",
            params![r.id, u.new, now_ms()],
        )?;
        let n = git(&["rev-list", "--count", &format!("{}..{}", u.old, u.new)], Some(&engine.git.path(repo.id))).await.unwrap_or_else(|_| "?".into());
        engine.db.add_review_event(r.id, None, "push", &format!("{n} new commit(s), now at {}", &u.new[..7]))?;
        if let Some(updated) = engine.db.review(r.id)? {
            crate::ai::on_review_updated(engine, &updated).await?;
        }
    }
    // A review whose branch was merged by hand into its target counts as merged.
    if !u.is_delete() {
        let path = engine.git.path(repo.id);
        let targeting = engine.db.reviews_where("v.repo_id = ?1 AND v.target_branch = ?2 AND v.state IN ('open', 'draft')", params![repo.id, branch])?;
        for r in targeting {
            if engine.git.is_ancestor(&path, &r.head_sha, &u.new).await {
                engine.db.conn().execute(
                    "UPDATE reviews SET state = 'merged', merge_sha = ?2, merged_at = ?3 WHERE id = ?1",
                    params![r.id, u.new, now_ms()],
                )?;
                engine.db.add_review_event(r.id, None, "merged", "Merged outside the review")?;
            }
        }
    }
    Ok(())
}

pub async fn on_run_finished(engine: &Arc<Engine>, ctx: &Arc<RunCtx>, status: Status) -> Result<()> {
    let Some(id) = ctx.git_ref.strip_prefix("refs/moloko/queue/").and_then(|x| x.parse::<i64>().ok()) else {
        return Ok(());
    };
    let Some(r) = engine.db.review(id)? else { return Ok(()) };
    if r.queue_state.as_deref() != Some("testing") || r.queue_run_id != Some(ctx.run_id) {
        return Ok(());
    }
    let (qsha, qbase): (String, String) =
        engine.db.conn().query_row("SELECT queue_sha, queue_base FROM reviews WHERE id = ?1", [id], |x| Ok((x.get(0)?, x.get(1)?)))?;
    let path = engine.git.path(r.repo_id);
    let _ = git(&["update-ref", "-d", &format!("refs/moloko/queue/{id}")], Some(&path)).await;
    if status != Status::Success {
        return fail_queue(engine, &r, &format!("Run #{} failed on the merge result", ctx.run_id));
    }
    match land(engine, &r, &qbase, &qsha, r.author_id).await {
        Ok(()) => {}
        Err(_) => {
            // The target moved meanwhile: test again on top of it.
            engine.db.conn().execute("UPDATE reviews SET queue_state = 'queued' WHERE id = ?1", [id])?;
            engine.db.add_review_event(id, None, "queue", "The target moved; testing again on top of it")?;
        }
    }
    engine.queue_notify.notify_one();
    Ok(())
}

/// The open review for a branch, so its runs can deploy a preview (`MOLOKO_REVIEW_ID`).
pub fn review_for_branch(db: &Db, repo_id: i64, git_ref: &str) -> Option<i64> {
    let branch = git_ref.strip_prefix("refs/heads/")?;
    db.active_reviews_for_branch(repo_id, branch).ok()?.first().map(|r| r.number)
}

// ---------- API ----------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects/{id}/reviews", get(list).post(create))
        .route("/api/projects/{id}/reviews/{number}", get(by_number))
        .route("/api/reviews/{id}", get(detail).patch(update))
        .route("/api/reviews/{id}/comments", post(comment))
        .route("/api/reviews/{id}/comments/{cid}/resolve", post(resolve))
        .route("/api/reviews/{id}/approve", post(approve))
        .route("/api/reviews/{id}/merge", post(merge_now))
        .route("/api/reviews/{id}/queue", delete(dequeue))
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
}

async fn list(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Query(q): Query<ListQuery>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    Ok(Json(s.engine.db.reviews(id, q.state.as_deref().unwrap_or("open"))?))
}

#[derive(Deserialize)]
pub struct NewReview {
    pub repo_id: i64,
    pub source_branch: String,
    pub target_branch: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
}

/// Open a review. Also used by AI agent sessions.
pub async fn open(engine: &Arc<Engine>, author: &User, n: &NewReview) -> Result<Review> {
    let repo = engine.db.repo(n.repo_id)?.ok_or_else(|| anyhow!("no such repo"))?;
    let path = engine.git.path(repo.id);
    let source = n.source_branch.trim().trim_start_matches("refs/heads/").to_string();
    let target = n.target_branch.clone().unwrap_or_else(|| repo.default_branch.clone());
    if source == target {
        bail!("source and target are the same branch");
    }
    let head = engine.git.resolve(&path, &format!("refs/heads/{source}")).await.map_err(|_| anyhow!("no branch {source}"))?;
    engine.git.resolve(&path, &format!("refs/heads/{target}")).await.map_err(|_| anyhow!("no branch {target}"))?;
    if let Some(existing) = engine.db.active_reviews_for_branch(repo.id, &source)?.into_iter().find(|r| r.target_branch == target) {
        return Ok(existing);
    }
    let project = engine.db.project(repo.project_id)?.ok_or_else(|| anyhow!("no project"))?;
    let issue = match crate::issues::find_keys(&source, &project.key).first() {
        Some(&k) => engine.db.issue_by_number(project.id, k)?,
        None => None,
    };
    let title = match n.title.clone().filter(|t| !t.trim().is_empty()) {
        Some(t) => t,
        None => match &issue {
            Some(i) => format!("{}: {}", i.key, i.title),
            None => git(&["log", "-1", "--format=%s", &head], Some(&path)).await.unwrap_or(source.clone()),
        },
    };
    let id = {
        let c = engine.db.conn();
        let number: i64 = c.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM reviews WHERE project_id = ?1", [project.id], |r| r.get(0))?;
        let now = now_ms();
        c.execute(
            "INSERT INTO reviews (project_id, repo_id, number, title, body, source_branch, target_branch, author_id, state, head_sha, issue_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![project.id, repo.id, number, title.trim(), n.body.clone().unwrap_or_default(), source, target, author.id,
                if n.draft { "draft" } else { "open" }, head, issue.as_ref().map(|i| i.id), now],
        )?;
        c.last_insert_rowid()
    };
    let review = engine.db.review(id)?.ok_or_else(|| anyhow!("review vanished"))?;
    engine.db.add_review_event(id, Some(author.id), "opened", &format!("{source} → {target}"))?;
    if let Some(i) = &issue {
        engine.db.set_issue_ref(i.id, "review", Some(repo.id), &format!("!{}", review.number), Some(&head), Some(json!({ "review_id": id })))?;
        if !n.draft {
            crate::issues::advance(&engine.db, i, "in_review", &format!("Review !{} opened", review.number))?;
        }
    }
    crate::ai::on_review_updated(engine, &review).await?;
    Ok(review)
}

async fn create(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(n): Json<NewReview>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Write)?;
    if s.engine.db.repo(n.repo_id)?.is_none_or(|r| r.project_id != id) {
        return Err(not_found());
    }
    let r = open(&s.engine, &actor.user, &n).await.map_err(|e| bad_request(format!("{e:#}")))?;
    Ok((StatusCode::CREATED, Json(r)))
}

async fn review_for(s: &AppState, actor: &Actor, id: i64, min: Role) -> ApiResult<Review> {
    let r = s.engine.db.review(id)?.ok_or_else(not_found)?;
    need(s, actor, r.project_id, min)?;
    Ok(r)
}

async fn detail_json(s: &AppState, r: Review) -> ApiResult<Value> {
    let path = s.engine.git.path(r.repo_id);
    let target = s.engine.git.resolve(&path, &format!("refs/heads/{}", r.target_branch)).await.unwrap_or_default();
    let base = match &r.merge_sha {
        // After merging, show what the review brought in.
        Some(m) if r.state == "merged" => git(&["rev-parse", &format!("{m}^1")], Some(&path)).await.unwrap_or_default(),
        _ => git(&["merge-base", &target, &r.head_sha], Some(&path)).await.unwrap_or_default(),
    };
    let (files, diff) = crate::code::diff(&path, &base, &r.head_sha).await.unwrap_or_default();
    let commits = crate::code::log(&path, &format!("{base}..{}", r.head_sha), 250).await.unwrap_or_default();
    let st = if matches!(r.state.as_str(), "open" | "draft") { Some(status(&s.engine, &r).await?) } else { None };
    let set = match r.issue_id {
        Some(i) => for_issue(&s.engine.db, i)?.into_iter().filter(|x| x.id != r.id).collect(),
        None => vec![],
    };
    Ok(json!({
        "review": r,
        "status": st,
        "files": files,
        "diff": diff,
        "commits": commits,
        "comments": s.engine.db.review_comments(r.id)?,
        "events": s.engine.db.review_events(r.id)?,
        "change_set": set,
    }))
}

async fn detail(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    let r = review_for(&s, &actor, id, Role::Read).await?;
    Ok(Json(detail_json(&s, r).await?))
}

async fn by_number(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path((id, number)): Path<(i64, i64)>) -> ApiResult<impl IntoResponse> {
    need(&s, &actor, id, Role::Read)?;
    let rid: i64 = s.engine.db.conn()
        .query_row("SELECT id FROM reviews WHERE project_id = ?1 AND number = ?2", params![id, number], |r| r.get(0))
        .optional()?
        .ok_or_else(not_found)?;
    let r = s.engine.db.review(rid)?.ok_or_else(not_found)?;
    Ok(Json(detail_json(&s, r).await?))
}

#[derive(Deserialize)]
struct ReviewPatch {
    title: Option<String>,
    body: Option<String>,
    /// `open`, `draft` or `closed`.
    state: Option<String>,
}

async fn update(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(p): Json<ReviewPatch>) -> ApiResult<impl IntoResponse> {
    let r = review_for(&s, &actor, id, Role::Write).await?;
    if r.state == "merged" {
        return Err(bad_request("merged reviews can't change"));
    }
    let c = s.engine.db.conn();
    if let Some(t) = p.title.filter(|t| !t.trim().is_empty()) {
        c.execute("UPDATE reviews SET title = ?2 WHERE id = ?1", params![id, t.trim()])?;
    }
    if let Some(b) = p.body {
        c.execute("UPDATE reviews SET body = ?2 WHERE id = ?1", params![id, b])?;
    }
    drop(c);
    if let Some(st) = p.state {
        if !["open", "draft", "closed"].contains(&st.as_str()) {
            return Err(bad_request("state must be open, draft or closed"));
        }
        if st != r.state {
            s.engine.db.conn().execute("UPDATE reviews SET state = ?2, queue_state = NULL WHERE id = ?1", params![id, st])?;
            s.engine.db.add_review_event(id, Some(actor.user.id), &st, &format!("Marked {st}"))?;
            if st == "open" && let Some(iid) = r.issue_id && let Some(i) = s.engine.db.issue(iid)? {
                crate::issues::advance(&s.engine.db, &i, "in_review", &format!("Review !{} ready", r.number))?;
            }
        }
    }
    Ok(Json(s.engine.db.review(id)?))
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
    path: Option<String>,
    line: Option<i64>,
    side: Option<String>,
    parent_id: Option<i64>,
}

async fn comment(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(c): Json<CommentBody>) -> ApiResult<impl IntoResponse> {
    let r = review_for(&s, &actor, id, Role::Write).await?;
    if c.body.trim().is_empty() {
        return Err(bad_request("comment is empty"));
    }
    let side = c.side.unwrap_or_else(|| "new".into());
    let anchor = match (&c.path, c.line) {
        (Some(p), Some(l)) => Some((p.as_str(), l, side.as_str(), r.head_sha.as_str())),
        _ => None,
    };
    let cid = s.engine.db.add_review_comment(id, actor.user.id, c.body.trim(), anchor, c.parent_id)?;
    if let Some(iid) = r.issue_id {
        s.engine.db.add_issue_event(iid, Some(actor.user.id), "review_comment", c.body.trim(), Some(json!({ "review": r.number })))?;
    }
    if let Some(iid) = r.issue_id && let Some(issue) = s.engine.db.issue(iid)? {
        crate::ai::on_mention(&s.engine, &issue, Some(&r), &actor.user, c.body.trim()).await?;
    } else {
        crate::ai::on_review_mention(&s.engine, &r, &actor.user, c.body.trim()).await?;
    }
    Ok((StatusCode::CREATED, Json(json!({ "id": cid }))))
}

#[derive(Deserialize)]
struct ResolveBody {
    resolved: bool,
}

async fn resolve(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path((id, cid)): Path<(i64, i64)>, Json(b): Json<ResolveBody>) -> ApiResult<impl IntoResponse> {
    review_for(&s, &actor, id, Role::Write).await?;
    let n = s.engine.db.conn().execute("UPDATE review_comments SET resolved = ?3 WHERE id = ?1 AND review_id = ?2", params![cid, id, b.resolved as i64])?;
    if n == 0 { Err(not_found()) } else { Ok(StatusCode::NO_CONTENT) }
}

#[derive(Deserialize)]
struct ApproveBody {
    /// `approved` or `changes_requested`.
    state: String,
    body: Option<String>,
}

async fn approve(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>, Json(b): Json<ApproveBody>) -> ApiResult<impl IntoResponse> {
    let r = review_for(&s, &actor, id, Role::Write).await?;
    if !["approved", "changes_requested"].contains(&b.state.as_str()) {
        return Err(bad_request("state must be approved or changes_requested"));
    }
    if r.author_id == Some(actor.user.id) {
        return Err(bad_request("you can't review your own change"));
    }
    s.engine.db.set_approval(id, actor.user.id, &b.state, &r.head_sha)?;
    let text = b.body.unwrap_or_default();
    s.engine.db.add_review_event(id, Some(actor.user.id), &b.state, &text)?;
    if b.state == "changes_requested" && r.author_kind.as_deref() == Some("bot") {
        crate::ai::on_changes_requested(&s.engine, &r, &actor.user, &text).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn merge_now(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    let r = review_for(&s, &actor, id, Role::Write).await?;
    if actor.user.kind == "bot" {
        return Err(ApiError(StatusCode::FORBIDDEN, "AI agents can't merge".into()));
    }
    let outcome = merge(&s.engine, &r, &actor.user).await.map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({ "result": outcome })))
}

async fn dequeue(State(s): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<i64>) -> ApiResult<impl IntoResponse> {
    review_for(&s, &actor, id, Role::Write).await?;
    s.engine.db.conn().execute("UPDATE reviews SET queue_state = NULL WHERE id = ?1", [id])?;
    s.engine.db.add_review_event(id, Some(actor.user.id), "dequeued", "Removed from the merge queue")?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codeowners() {
        let file = "# owners\n*.rs @rust-team\n/docs/ @writer\nserver/** @dani\n";
        assert_eq!(owners_of(file, "src/main.rs"), vec!["rust-team"]);
        assert_eq!(owners_of(file, "docs/intro.md"), vec!["writer"]);
        assert_eq!(owners_of(file, "server/src/a.rs"), vec!["dani"]);
        assert!(owners_of(file, "README.md").is_empty());
        assert!(owner_pattern_matches("api/", "api/x/y.go"));
        assert!(!owner_pattern_matches("/api/", "web/api/y.go"));
    }
}
