//! Repositories inside a project, and the one path every ref change goes through:
//! policy checks, write-through to GitHub, the atomic ref update, then the follow-ups
//! (runs, issue links, reviews, outbound mirroring).

use crate::auth::{Actor, Role};
use crate::db::{Db, now_ms};
use crate::engine::Engine;
use crate::git::{self, ZERO_SHA, is_zero};
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::sync::Arc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    default_branch TEXT NOT NULL DEFAULT 'main',
    upstream_url TEXT,
    mirror_mode TEXT NOT NULL DEFAULT 'none',
    on_unreachable TEXT NOT NULL DEFAULT 'reject',
    credential TEXT,
    protect_default INTEGER NOT NULL DEFAULT 1,
    require_review INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE (project_id, name)
);
CREATE TABLE IF NOT EXISTS mirror_refs (
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    ref TEXT NOT NULL,
    local_sha TEXT,
    remote_sha TEXT,
    state TEXT NOT NULL,
    last_error TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, ref)
);
"#;

/// How a repo relates to its upstream (usually GitHub).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MirrorMode {
    /// Hosted in moloko only.
    None,
    /// Upstream is the source of truth; moloko fetches and refuses pushes.
    Pull,
    /// moloko is the source of truth and pushes every change upstream.
    Push,
    /// Both accept writes. Pushes to moloko are written to upstream first.
    Twoway,
}

impl MirrorMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => MirrorMode::None,
            "pull" => MirrorMode::Pull,
            "push" => MirrorMode::Push,
            "twoway" => MirrorMode::Twoway,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            MirrorMode::None => "none",
            MirrorMode::Pull => "pull",
            MirrorMode::Push => "push",
            MirrorMode::Twoway => "twoway",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub default_branch: String,
    pub upstream_url: Option<String>,
    pub mirror_mode: MirrorMode,
    /// Two-way mode when upstream can't be reached: `reject` or `queue`.
    pub on_unreachable: String,
    pub has_credential: bool,
    pub protect_default: bool,
    pub require_review: bool,
    pub created_at: i64,
}

const REPO_COLS: &str = "id, project_id, name, default_branch, upstream_url, mirror_mode, on_unreachable, credential IS NOT NULL, protect_default, require_review, created_at";

fn map_repo(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    let mode: String = r.get(5)?;
    Ok(Repo {
        id: r.get(0)?,
        project_id: r.get(1)?,
        name: r.get(2)?,
        default_branch: r.get(3)?,
        upstream_url: r.get(4)?,
        mirror_mode: MirrorMode::parse(&mode).unwrap_or(MirrorMode::None),
        on_unreachable: r.get(6)?,
        has_credential: r.get(7)?,
        protect_default: r.get::<_, i64>(8)? != 0,
        require_review: r.get::<_, i64>(9)? != 0,
        created_at: r.get(10)?,
    })
}

/// `My API` -> `my-api`.
pub fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() { "repo".into() } else { s }
}

pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[derive(Debug, Clone, Serialize)]
pub struct MirrorRef {
    #[serde(rename = "ref")]
    pub name: String,
    pub local_sha: Option<String>,
    pub remote_sha: Option<String>,
    /// `in_sync`, `pending`, `diverged` or `error`.
    pub state: String,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

#[derive(Default)]
pub struct RepoSettings {
    pub default_branch: Option<String>,
    pub upstream_url: Option<Option<String>>,
    pub mirror_mode: Option<MirrorMode>,
    pub on_unreachable: Option<String>,
    pub credential: Option<Option<String>>,
    pub protect_default: Option<bool>,
    pub require_review: Option<bool>,
}

impl Db {
    pub fn create_repo(
        &self,
        project_id: i64,
        name: &str,
        upstream_url: Option<&str>,
        mode: MirrorMode,
    ) -> Result<i64> {
        if !valid_repo_name(name) {
            bail!("repo names may only contain letters, digits, '-', '_' and '.'");
        }
        if mode != MirrorMode::None && upstream_url.is_none_or(str::is_empty) {
            bail!("mirror mode {} needs an upstream URL", mode.as_str());
        }
        let c = self.conn();
        c.execute(
            "INSERT INTO repos (project_id, name, upstream_url, mirror_mode, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, name, upstream_url, mode.as_str(), now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_repo(&self, id: i64, s: RepoSettings) -> Result<()> {
        let c = self.conn();
        if let Some(v) = s.default_branch {
            c.execute("UPDATE repos SET default_branch = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = s.upstream_url {
            c.execute("UPDATE repos SET upstream_url = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = s.mirror_mode {
            c.execute("UPDATE repos SET mirror_mode = ?2 WHERE id = ?1", params![id, v.as_str()])?;
        }
        if let Some(v) = s.on_unreachable {
            c.execute("UPDATE repos SET on_unreachable = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = s.credential {
            c.execute("UPDATE repos SET credential = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = s.protect_default {
            c.execute("UPDATE repos SET protect_default = ?2 WHERE id = ?1", params![id, v as i64])?;
        }
        if let Some(v) = s.require_review {
            c.execute("UPDATE repos SET require_review = ?2 WHERE id = ?1", params![id, v as i64])?;
        }
        Ok(())
    }

    pub fn delete_repo(&self, id: i64) -> Result<bool> {
        let c = self.conn();
        c.execute("UPDATE runs SET repo_id = NULL WHERE repo_id = ?1", [id])?;
        Ok(c.execute("DELETE FROM repos WHERE id = ?1", [id])? > 0)
    }

    pub fn repos(&self, project_id: i64) -> Result<Vec<Repo>> {
        let c = self.conn();
        let mut st = c.prepare(&format!(
            "SELECT {REPO_COLS} FROM repos WHERE project_id = ?1 ORDER BY id"
        ))?;
        let rows = st.query_map([project_id], map_repo)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn all_repos(&self) -> Result<Vec<Repo>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {REPO_COLS} FROM repos ORDER BY id"))?;
        let rows = st.query_map([], map_repo)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn repo(&self, id: i64) -> Result<Option<Repo>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {REPO_COLS} FROM repos WHERE id = ?1"),
            [id],
            map_repo,
        )
        .optional()?)
    }

    pub fn repo_by_name(&self, project_id: i64, name: &str) -> Result<Option<Repo>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {REPO_COLS} FROM repos WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE"),
            params![project_id, name],
            map_repo,
        )
        .optional()?)
    }

    pub fn repo_credential(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT credential FROM repos WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn set_mirror_ref(
        &self,
        repo_id: i64,
        name: &str,
        local: Option<&str>,
        remote: Option<&str>,
        state: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO mirror_refs (repo_id, ref, local_sha, remote_sha, state, last_error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (repo_id, ref) DO UPDATE SET local_sha = excluded.local_sha, remote_sha = excluded.remote_sha,
               state = excluded.state, last_error = excluded.last_error, updated_at = excluded.updated_at",
            params![repo_id, name, local, remote, state, error, now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_mirror_ref(&self, repo_id: i64, name: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM mirror_refs WHERE repo_id = ?1 AND ref = ?2",
            params![repo_id, name],
        )?;
        Ok(())
    }

    pub fn mirror_refs(&self, repo_id: i64) -> Result<Vec<MirrorRef>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT ref, local_sha, remote_sha, state, last_error, updated_at FROM mirror_refs WHERE repo_id = ?1 ORDER BY ref",
        )?;
        let rows = st.query_map([repo_id], |r| {
            Ok(MirrorRef {
                name: r.get(0)?,
                local_sha: r.get(1)?,
                remote_sha: r.get(2)?,
                state: r.get(3)?,
                last_error: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// One ref change. An all-zero sha means "doesn't exist" (create / delete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: String,
    pub old: String,
    pub new: String,
}

impl RefUpdate {
    pub fn is_delete(&self) -> bool {
        is_zero(&self.new)
    }
    pub fn branch(&self) -> Option<&str> {
        self.name.strip_prefix("refs/heads/")
    }
}

/// Where a ref change comes from; decides which checks apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `git push` from a user or an AI agent.
    Push,
    /// moloko itself: a review merge, the merge queue, an agent session.
    Internal,
}

impl Engine {
    /// Check, write through, and apply ref updates atomically. Returns one result per update,
    /// in order: `Ok` or the rejection reason shown to the pusher.
    pub async fn apply_ref_updates(
        self: &Arc<Self>,
        repo: &crate::repos::Repo,
        actor: Option<&Actor>,
        updates: &[RefUpdate],
        source: Source,
        atomic: bool,
    ) -> Result<Vec<Result<(), String>>> {
        let path = self.git.ensure(repo.id, &repo.default_branch).await?;
        let lock = self.git.lock(repo.id);
        let _g = lock.lock().await;
        let mut results: Vec<Result<(), String>> = Vec::with_capacity(updates.len());
        for u in updates {
            results.push(self.check_update(repo, actor, u, source, &path).await);
        }
        if atomic && results.iter().any(|r| r.is_err()) {
            for r in results.iter_mut().filter(|r| r.is_ok()) {
                *r = Err("atomic push failed".into());
            }
            return Ok(results);
        }

        // Two-way mirroring: upstream must accept first, so the two never diverge.
        if repo.mirror_mode == MirrorMode::Twoway {
            let accepted: Vec<&RefUpdate> = updates
                .iter()
                .zip(&results)
                .filter(|(_, r)| r.is_ok())
                .map(|(u, _)| u)
                .collect();
            if !accepted.is_empty() {
                let outcome = crate::mirror::push_upstream(self, repo, &path, &accepted).await;
                for (u, r) in updates.iter().zip(results.iter_mut()) {
                    if r.is_err() {
                        continue;
                    }
                    match &outcome {
                        Ok(per_ref) => {
                            if let Some(Err(msg)) = per_ref.get(&u.name) {
                                *r = Err(msg.clone());
                            }
                        }
                        Err(e) if repo.on_unreachable == "queue" => {
                            let _ = self.db.set_mirror_ref(
                                repo.id,
                                &u.name,
                                Some(&u.new),
                                None,
                                "pending",
                                Some(&format!("{e:#}")),
                            );
                        }
                        Err(e) => *r = Err(format!("upstream unreachable, push rejected: {e:#}")),
                    }
                }
            }
        }

        let accepted: Vec<&RefUpdate> = updates
            .iter()
            .zip(&results)
            .filter(|(_, r)| r.is_ok())
            .map(|(u, _)| u)
            .collect();
        if !accepted.is_empty() {
            let mut tx = String::new();
            for u in &accepted {
                if u.is_delete() {
                    tx.push_str(&format!("delete {} {}\n", u.name, u.old));
                } else {
                    tx.push_str(&format!("update {} {} {}\n", u.name, u.new, u.old));
                }
            }
            let out = git::git_output(&["update-ref", "--stdin"], Some(&path), &[], Some(tx.as_bytes())).await?;
            if !out.status.success() {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                for (u, r) in updates.iter().zip(results.iter_mut()) {
                    if accepted.iter().any(|a| a.name == u.name) {
                        *r = Err(format!("failed to update ref: {msg}"));
                    }
                }
                return Ok(results);
            }
            let applied: Vec<RefUpdate> = accepted.into_iter().cloned().collect();
            let this = self.clone();
            let repo = repo.clone();
            let actor_id = actor.map(|a| a.user.id);
            drop(_g);
            this.after_ref_updates(&repo, actor_id, &applied).await;
        }
        Ok(results)
    }

    async fn check_update(
        &self,
        repo: &Repo,
        actor: Option<&Actor>,
        u: &RefUpdate,
        source: Source,
        path: &std::path::Path,
    ) -> Result<(), String> {
        if !(u.name.starts_with("refs/heads/") || u.name.starts_with("refs/tags/"))
            && source == Source::Push
        {
            return Err("only branches and tags can be pushed".into());
        }
        if source == Source::Internal {
            return Ok(());
        }
        if repo.mirror_mode == MirrorMode::Pull {
            return Err(format!(
                "this repo mirrors {}; push there instead",
                repo.upstream_url.as_deref().unwrap_or("upstream")
            ));
        }
        if let Some(a) = actor {
            if !a.may_push(repo.project_id, &u.name) {
                return Err("this token may not push to this ref".into());
            }
        }
        let is_default = u.branch() == Some(repo.default_branch.as_str());
        if is_default && repo.protect_default {
            if u.is_delete() {
                return Err("the default branch is protected and can't be deleted".into());
            }
            if !is_zero(&u.old) && !self.git.is_ancestor(path, &u.old, &u.new).await {
                return Err("the default branch is protected: force-push rejected".into());
            }
        }
        if is_default && repo.require_review && !is_zero(&u.old) {
            let admin = match actor {
                Some(a) => matches!(self.db.role(&a.user, repo.project_id), Ok(Some(Role::Admin))),
                None => false,
            };
            if !admin {
                return Err("changes to the default branch need a review".into());
            }
        }
        Ok(())
    }

    /// Everything that follows an applied ref change. Also called for changes fetched from upstream.
    pub async fn after_ref_updates(
        self: &Arc<Self>,
        repo: &Repo,
        actor_id: Option<i64>,
        updates: &[RefUpdate],
    ) {
        let path = self.git.path(repo.id);
        if repo.mirror_mode == MirrorMode::Push {
            let this = self.clone();
            let (repo2, ups) = (repo.clone(), updates.to_vec());
            tokio::spawn(async move {
                let refs: Vec<&RefUpdate> = ups.iter().collect();
                crate::mirror::push_and_record(&this, &repo2, &refs).await;
            });
        }
        for u in updates {
            if let Err(e) = crate::issues::on_ref_update(self, repo, actor_id, u, &path).await {
                tracing::warn!(repo = repo.name, "issue automation failed: {e:#}");
            }
            if let Err(e) = crate::reviews::on_ref_update(self, repo, u).await {
                tracing::warn!(repo = repo.name, "review update failed: {e:#}");
            }
            if u.is_delete() || u.name.starts_with("refs/moloko/") {
                continue;
            }
            // Only refs with a pipeline get a run, so plain hosted repos don't fill the list with failures.
            if self
                .git
                .read_file(&path, &u.new, crate::config::PIPELINE_FILE)
                .await
                .is_ok()
            {
                let trigger = if actor_id.is_some() { "push" } else { "mirror" };
                if let Err(e) = self.trigger(repo.id, &u.name, Some(&u.new), trigger) {
                    tracing::warn!(repo = repo.name, "trigger failed: {e:#}");
                }
            }
        }
    }
}

pub fn zero() -> String {
    ZERO_SHA.to_string()
}
