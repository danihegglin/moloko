//! Mirroring with an upstream (usually GitHub).
//!
//! - `pull`: fetch upstream branches and tags into the repo; moloko refuses pushes.
//! - `push`: after moloko accepts a change, push it upstream; GitHub is a read-only copy.
//! - `twoway`: pushes to moloko are written to upstream first (see `apply_ref_updates`), and
//!   changes pushed to upstream are fast-forwarded in. A ref is never overwritten with history
//!   it doesn't contain: if both sides moved, the ref is marked `diverged` and left alone.

use crate::engine::Engine;
use crate::git::{self, ZERO_SHA, is_zero};
use crate::repos::{MirrorMode, RefUpdate, Repo};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Credentials for upstream as `GIT_CONFIG_*` env vars, so they never appear in argv or on disk.
pub(crate) fn cred_env(engine: &Engine, repo: &Repo) -> Result<Vec<(String, String)>> {
    let Some(sealed) = engine.db.repo_credential(repo.id)? else {
        return Ok(vec![]);
    };
    let token = engine.secrets.open(&sealed)?;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    Ok(vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
        ("GIT_CONFIG_VALUE_0".into(), format!("Authorization: Basic {basic}")),
    ])
}

fn upstream(repo: &Repo) -> Result<&str> {
    repo.upstream_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("repo {} has no upstream URL", repo.name))
}

/// Push `updates` upstream with a lease on each ref, so nothing upstream is overwritten that we
/// haven't seen. `Err` means upstream couldn't be reached at all; per-ref rejections are in the map.
pub async fn push_upstream(
    engine: &Engine,
    repo: &Repo,
    path: &Path,
    updates: &[&RefUpdate],
) -> Result<HashMap<String, Result<(), String>>> {
    let url = upstream(repo)?.to_string();
    let env = cred_env(engine, repo)?;
    let known: HashMap<String, Option<String>> = engine
        .db
        .mirror_refs(repo.id)?
        .into_iter()
        .map(|m| (m.name, m.remote_sha))
        .collect();
    let mut args: Vec<String> = vec!["push".into(), "--porcelain".into(), url];
    for u in updates {
        // Two-way: upstream must still be at our old value. Push: at what we last pushed.
        let expect = match (repo.mirror_mode, known.get(&u.name)) {
            (MirrorMode::Push, Some(Some(remote))) => remote.clone(),
            _ => u.old.clone(),
        };
        let expect = if is_zero(&expect) { String::new() } else { expect };
        args.push(format!("--force-with-lease={}:{}", u.name, expect));
    }
    for u in updates {
        if u.is_delete() {
            args.push(format!(":{}", u.name));
        } else {
            args.push(format!("{}:{}", u.new, u.name));
        }
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git::git_output(&argv, Some(path), &env, None).await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut result = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let (Some(flag), Some(spec), summary) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some((_, dst)) = spec.split_once(':') else {
            continue;
        };
        let r = if flag == "!" {
            let why = summary.unwrap_or("rejected");
            Err(if why.contains("stale info") || why.contains("fetch first") {
                "upstream has changes you don't have: fetch first".to_string()
            } else {
                format!("upstream rejected: {why}")
            })
        } else {
            Ok(())
        };
        result.insert(dst.to_string(), r);
    }
    if result.is_empty() && !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(result)
}

/// Push mode: push changes and record the outcome per ref.
pub async fn push_and_record(engine: &Arc<Engine>, repo: &Repo, updates: &[&RefUpdate]) {
    let path = engine.git.path(repo.id);
    match push_upstream(engine, repo, &path, updates).await {
        Ok(per_ref) => {
            for u in updates {
                match per_ref.get(&u.name) {
                    Some(Err(e)) => {
                        let _ = engine.db.set_mirror_ref(repo.id, &u.name, Some(&u.new), None, "error", Some(e));
                    }
                    _ if u.is_delete() => {
                        let _ = engine.db.delete_mirror_ref(repo.id, &u.name);
                    }
                    _ => {
                        let _ = engine.db.set_mirror_ref(repo.id, &u.name, Some(&u.new), Some(&u.new), "in_sync", None);
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(repo = repo.name, "mirror push failed: {e:#}");
            for u in updates {
                let _ = engine.db.set_mirror_ref(repo.id, &u.name, Some(&u.new), None, "pending", Some(&format!("{e:#}")));
            }
        }
    }
}

fn diff_refs(before: &HashMap<String, String>, after: &HashMap<String, String>) -> Vec<RefUpdate> {
    let mut out = Vec::new();
    for (name, new) in after {
        let old = before.get(name).cloned().unwrap_or_else(|| ZERO_SHA.into());
        if &old != new {
            out.push(RefUpdate { name: name.clone(), old, new: new.clone() });
        }
    }
    for (name, old) in before {
        if !after.contains_key(name) {
            out.push(RefUpdate { name: name.clone(), old: old.clone(), new: ZERO_SHA.into() });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Bring the repo up to date with upstream, according to its mirror mode.
/// Returns the ref changes applied locally.
pub async fn sync(engine: &Arc<Engine>, repo: &Repo) -> Result<Vec<RefUpdate>> {
    match repo.mirror_mode {
        MirrorMode::Pull => fetch_pull(engine, repo).await,
        MirrorMode::Twoway => fetch_twoway(engine, repo).await,
        MirrorMode::Push => {
            retry_pending(engine, repo).await;
            Ok(vec![])
        }
        MirrorMode::None => Ok(vec![]),
    }
}

async fn fetch_pull(engine: &Arc<Engine>, repo: &Repo) -> Result<Vec<RefUpdate>> {
    let path = engine.git.ensure(repo.id, &repo.default_branch).await?;
    let env = cred_env(engine, repo)?;
    let lock = engine.git.lock(repo.id);
    let guard = lock.lock().await;
    let before = engine.git.refs(&path).await?;
    git::git_env(
        &[
            "fetch",
            "--prune",
            "--quiet",
            "--no-write-fetch-head",
            upstream(repo)?,
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
        ],
        Some(&path),
        &env,
    )
    .await
    .context("fetch from upstream")?;
    let after = engine.git.refs(&path).await?;
    drop(guard);
    // Follow upstream's default branch on first fetch (e.g. `master`).
    if before.is_empty() && !after.contains_key(&format!("refs/heads/{}", repo.default_branch)) {
        if let Ok(head) = git::git_env(&["ls-remote", "--symref", upstream(repo)?, "HEAD"], Some(&path), &env).await {
            if let Some(branch) = head
                .lines()
                .find_map(|l| l.strip_prefix("ref: refs/heads/"))
                .and_then(|l| l.split('\t').next())
            {
                let _ = engine.db.update_repo(repo.id, crate::repos::RepoSettings {
                    default_branch: Some(branch.to_string()),
                    ..Default::default()
                });
                let _ = git::git(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")], Some(&path)).await;
            }
        }
    }
    let updates = diff_refs(&before, &after);
    for u in &updates {
        if u.is_delete() {
            let _ = engine.db.delete_mirror_ref(repo.id, &u.name);
        } else {
            let _ = engine.db.set_mirror_ref(repo.id, &u.name, Some(&u.new), Some(&u.new), "in_sync", None);
        }
    }
    let repo = engine.db.repo(repo.id)?.unwrap_or_else(|| repo.clone());
    // The first fetch of an existing repo would start a run for every branch; skip that.
    if !before.is_empty() {
        engine.after_ref_updates(&repo, None, &updates).await;
    }
    Ok(updates)
}

/// Two-way inbound: fetch upstream into `refs/github/*`, then fast-forward local refs.
async fn fetch_twoway(engine: &Arc<Engine>, repo: &Repo) -> Result<Vec<RefUpdate>> {
    let path = engine.git.ensure(repo.id, &repo.default_branch).await?;
    let env = cred_env(engine, repo)?;
    let lock = engine.git.lock(repo.id);
    let guard = lock.lock().await;
    git::git_env(
        &[
            "fetch",
            "--prune",
            "--quiet",
            "--no-write-fetch-head",
            upstream(repo)?,
            "+refs/heads/*:refs/github/heads/*",
            "+refs/tags/*:refs/github/tags/*",
        ],
        Some(&path),
        &env,
    )
    .await
    .context("fetch from upstream")?;
    let local = engine.git.refs(&path).await?;
    let remote: HashMap<String, String> = git::git(
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs/github"],
        Some(&path),
    )
    .await?
    .lines()
    .filter_map(|l| l.split_once(' '))
    .map(|(r, s)| (r.replacen("refs/github/", "refs/", 1), s.to_string()))
    .collect();
    let known: HashMap<String, crate::repos::MirrorRef> = engine
        .db
        .mirror_refs(repo.id)?
        .into_iter()
        .map(|m| (m.name.clone(), m))
        .collect();

    let mut apply = Vec::new();
    let mut push_out = Vec::new();
    for (name, r) in &remote {
        match local.get(name) {
            None => {
                // New upstream, unless we deleted it locally while in sync.
                apply.push(RefUpdate { name: name.clone(), old: ZERO_SHA.into(), new: r.clone() });
            }
            Some(l) if l == r => {
                engine.db.set_mirror_ref(repo.id, name, Some(l), Some(r), "in_sync", None)?;
            }
            Some(l) => {
                if engine.git.is_ancestor(&path, l, r).await {
                    apply.push(RefUpdate { name: name.clone(), old: l.clone(), new: r.clone() });
                } else if engine.git.is_ancestor(&path, r, l).await {
                    push_out.push(RefUpdate { name: name.clone(), old: r.clone(), new: l.clone() });
                } else {
                    engine.db.set_mirror_ref(
                        repo.id,
                        name,
                        Some(l),
                        Some(r),
                        "diverged",
                        Some("moloko and upstream both have commits the other lacks"),
                    )?;
                }
            }
        }
    }
    for (name, l) in &local {
        if remote.contains_key(name) {
            continue;
        }
        // Deleted upstream after being in sync: delete here too. Otherwise it's new here: push it.
        match known.get(name) {
            Some(k) if k.state == "in_sync" && k.remote_sha.as_deref() == Some(l) => {
                apply.push(RefUpdate { name: name.clone(), old: l.clone(), new: ZERO_SHA.into() })
            }
            _ => push_out.push(RefUpdate { name: name.clone(), old: ZERO_SHA.into(), new: l.clone() }),
        }
    }

    if !apply.is_empty() {
        let mut tx = String::new();
        for u in &apply {
            if u.is_delete() {
                tx.push_str(&format!("delete {} {}\n", u.name, u.old));
            } else {
                tx.push_str(&format!("update {} {} {}\n", u.name, u.new, u.old));
            }
        }
        let out = git::git_output(&["update-ref", "--stdin"], Some(&path), &[], Some(tx.as_bytes())).await?;
        if !out.status.success() {
            bail!("update refs from upstream: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        for u in &apply {
            if u.is_delete() {
                engine.db.delete_mirror_ref(repo.id, &u.name)?;
            } else {
                engine.db.set_mirror_ref(repo.id, &u.name, Some(&u.new), Some(&u.new), "in_sync", None)?;
            }
        }
    }
    drop(guard);
    if !push_out.is_empty() {
        let refs: Vec<&RefUpdate> = push_out.iter().collect();
        push_and_record(engine, repo, &refs).await;
    }
    engine.after_ref_updates(repo, None, &apply).await;
    Ok(apply)
}

/// Push mode: retry refs whose last push failed.
async fn retry_pending(engine: &Arc<Engine>, repo: &Repo) {
    let Ok(refs) = engine.db.mirror_refs(repo.id) else { return };
    let path = engine.git.path(repo.id);
    let Ok(local) = engine.git.refs(&path).await else { return };
    let retry: Vec<RefUpdate> = refs
        .into_iter()
        .filter(|m| m.state == "pending" || m.state == "error")
        .map(|m| RefUpdate {
            new: local.get(&m.name).cloned().unwrap_or_else(|| ZERO_SHA.into()),
            old: m.remote_sha.clone().unwrap_or_else(|| ZERO_SHA.into()),
            name: m.name,
        })
        .collect();
    if !retry.is_empty() {
        let refs: Vec<&RefUpdate> = retry.iter().collect();
        push_and_record(engine, repo, &refs).await;
    }
}

/// Settle a diverged ref by keeping one side. The other side's commits stay reachable in
/// `refs/moloko/diverged/…` so nothing is lost.
pub async fn resolve_diverged(engine: &Arc<Engine>, repo: &Repo, name: &str, keep: &str) -> Result<()> {
    let path = engine.git.path(repo.id);
    let refs = engine.db.mirror_refs(repo.id)?;
    let m = refs
        .into_iter()
        .find(|m| m.name == name && m.state == "diverged")
        .ok_or_else(|| anyhow!("{name} is not diverged"))?;
    let (local, remote) = (m.local_sha.unwrap_or_default(), m.remote_sha.unwrap_or_default());
    let backup = format!("refs/moloko/diverged/{}/{}", if keep == "moloko" { "upstream" } else { "moloko" }, name.trim_start_matches("refs/"));
    match keep {
        "moloko" => {
            git::git(&["update-ref", &backup, &remote], Some(&path)).await?;
            let env = cred_env(engine, repo)?;
            git::git_env(
                &["push", upstream(repo)?, &format!("--force-with-lease={name}:{remote}"), &format!("{local}:{name}")],
                Some(&path),
                &env,
            )
            .await?;
            engine.db.set_mirror_ref(repo.id, name, Some(&local), Some(&local), "in_sync", None)?;
        }
        "upstream" => {
            git::git(&["update-ref", &backup, &local], Some(&path)).await?;
            git::git(&["update-ref", name, &remote, &local], Some(&path)).await?;
            engine.db.set_mirror_ref(repo.id, name, Some(&remote), Some(&remote), "in_sync", None)?;
            let u = RefUpdate { name: name.to_string(), old: local, new: remote };
            engine.after_ref_updates(repo, None, &[u]).await;
        }
        _ => bail!("keep must be `moloko` or `upstream`"),
    }
    Ok(())
}

/// Poll upstreams in the background: pull and two-way repos fetch, push repos retry failures.
pub fn spawn_loop(engine: Arc<Engine>, every: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let Ok(repos) = engine.db.all_repos() else { continue };
            for repo in repos {
                if repo.mirror_mode == MirrorMode::None {
                    continue;
                }
                if let Err(e) = sync(&engine, &repo).await {
                    tracing::debug!(repo = repo.name, "mirror sync failed: {e:#}");
                }
            }
        }
    });
}
