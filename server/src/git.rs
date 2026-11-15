//! Local git mirror per repo. Jobs check out via `git worktree`, so a checkout
//! costs a few file writes instead of a network clone.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::process::Command;

pub struct GitCache {
    root: PathBuf,
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
}

async fn git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd.output().await.context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl GitCache {
    pub fn new(root: &Path) -> Self {
        GitCache {
            root: root.to_path_buf(),
            locks: Mutex::default(),
        }
    }

    pub(crate) fn lock(&self, repo_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(repo_id)
            .or_default()
            .clone()
    }

    pub fn mirror_path(&self, repo_id: i64) -> PathBuf {
        self.root.join(format!("{repo_id}.git"))
    }

    /// Clone the mirror on first use, otherwise fetch only what changed.
    pub async fn sync(&self, repo_id: i64, url: &str) -> Result<PathBuf> {
        let lock = self.lock(repo_id);
        let _g = lock.lock().await;
        let mirror = self.mirror_path(repo_id);
        if mirror.join("HEAD").exists() {
            git(&["fetch", "--prune", "--quiet", "origin"], Some(&mirror)).await?;
        } else {
            let tmp = self.root.join(format!("{repo_id}.git.tmp"));
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            git(
                &["clone", "--mirror", "--quiet", url, tmp.to_str().unwrap()],
                None,
            )
            .await?;
            tokio::fs::rename(&tmp, &mirror).await?;
        }
        Ok(mirror)
    }

    pub async fn resolve(&self, mirror: &Path, git_ref: &str) -> Result<String> {
        for candidate in [
            git_ref.to_string(),
            format!("refs/heads/{git_ref}"),
            format!("refs/tags/{git_ref}"),
        ] {
            let spec = format!("{candidate}^{{commit}}");
            if let Ok(sha) = git(&["rev-parse", "--verify", "--quiet", &spec], Some(mirror)).await {
                return Ok(sha);
            }
        }
        bail!("ref {git_ref:?} not found")
    }

    pub async fn read_file(&self, mirror: &Path, sha: &str, path: &str) -> Result<String> {
        git(&["show", &format!("{sha}:{path}")], Some(mirror)).await
    }

    /// File names directly inside `dir` at `sha` (empty if the directory doesn't exist).
    pub async fn list_dir(&self, mirror: &Path, sha: &str, dir: &str) -> Vec<String> {
        git(&["ls-tree", "--name-only", &format!("{sha}:{dir}")], Some(mirror))
            .await
            .map(|o| o.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// Files changed between two commits; None if `before` is unknown (e.g. a new branch).
    pub async fn changed_files(&self, mirror: &Path, before: &str, after: &str) -> Option<Vec<String>> {
        if before.chars().all(|c| c == '0') {
            return None;
        }
        git(&["diff", "--name-only", before, after], Some(mirror))
            .await
            .ok()
            .map(|o| o.lines().map(str::to_string).collect())
    }

    /// Expand `main`, `v1.0` or `HEAD` to a full ref (`refs/heads/main`); empty for a bare sha.
    pub async fn full_ref(&self, mirror: &Path, git_ref: &str) -> String {
        if git_ref.starts_with("refs/") {
            return git_ref.to_string();
        }
        if git_ref == "HEAD" {
            return git(&["symbolic-ref", "HEAD"], Some(mirror)).await.unwrap_or_default();
        }
        for candidate in [format!("refs/heads/{git_ref}"), format!("refs/tags/{git_ref}")] {
            if git(&["show-ref", "--verify", "--quiet", &candidate], Some(mirror)).await.is_ok() {
                return candidate;
            }
        }
        String::new()
    }

    pub async fn checkout(&self, repo_id: i64, sha: &str, dest: &Path) -> Result<()> {
        let lock = self.lock(repo_id);
        let _g = lock.lock().await;
        let mirror = self.mirror_path(repo_id);
        git(
            &[
                "worktree",
                "add",
                "--detach",
                "--force",
                dest.to_str().unwrap(),
                sha,
            ],
            Some(&mirror),
        )
        .await?;
        Ok(())
    }

    /// Delete the repo's mirror, e.g. after its URL changed or the repo was deleted.
    pub async fn forget(&self, repo_id: i64) {
        let lock = self.lock(repo_id);
        let _g = lock.lock().await;
        let _ = tokio::fs::remove_dir_all(self.mirror_path(repo_id)).await;
    }

    pub async fn remove(&self, repo_id: i64, dest: &Path) {
        let lock = self.lock(repo_id);
        let _g = lock.lock().await;
        let mirror = self.mirror_path(repo_id);
        if git(
            &["worktree", "remove", "--force", dest.to_str().unwrap()],
            Some(&mirror),
        )
        .await
        .is_err()
        {
            let _ = tokio::fs::remove_dir_all(dest).await;
            let _ = git(&["worktree", "prune"], Some(&mirror)).await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Create a repo with one commit containing `files`; returns its path.
    pub async fn fixture_repo(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        let repo = dir.join("origin");
        std::fs::create_dir_all(&repo).unwrap();
        git(&["init", "--quiet", "-b", "main"], Some(&repo))
            .await
            .unwrap();
        for (name, content) in files {
            let p = repo.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        git(&["add", "."], Some(&repo)).await.unwrap();
        git(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
            Some(&repo),
        )
        .await
        .unwrap();
        repo
    }

    #[tokio::test]
    async fn mirror_resolve_and_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture_repo(dir.path(), &[("a.txt", "hello")]).await;
        let cache = GitCache::new(&dir.path().join("mirrors"));
        std::fs::create_dir_all(dir.path().join("mirrors")).unwrap();
        let mirror = cache.sync(1, repo.to_str().unwrap()).await.unwrap();
        cache.sync(1, repo.to_str().unwrap()).await.unwrap(); // second sync fetches
        let sha = cache.resolve(&mirror, "main").await.unwrap();
        assert_eq!(
            cache.read_file(&mirror, &sha, "a.txt").await.unwrap(),
            "hello"
        );
        let ws = dir.path().join("ws");
        cache.checkout(1, &sha, &ws).await.unwrap();
        assert!(ws.join("a.txt").exists());
        cache.remove(1, &ws).await;
        assert!(!ws.exists());
    }
}
