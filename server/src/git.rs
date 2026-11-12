//! Local git mirror per project. Jobs check out via `git worktree`, so a checkout
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

    fn lock(&self, project_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(project_id)
            .or_default()
            .clone()
    }

    pub fn mirror_path(&self, project_id: i64) -> PathBuf {
        self.root.join(format!("{project_id}.git"))
    }

    /// Clone the mirror on first use, otherwise fetch only what changed.
    pub async fn sync(&self, project_id: i64, url: &str) -> Result<PathBuf> {
        let lock = self.lock(project_id);
        let _g = lock.lock().await;
        let mirror = self.mirror_path(project_id);
        if mirror.join("HEAD").exists() {
            git(&["fetch", "--prune", "--quiet", "origin"], Some(&mirror)).await?;
        } else {
            let tmp = self.root.join(format!("{project_id}.git.tmp"));
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

    pub async fn checkout(&self, project_id: i64, sha: &str, dest: &Path) -> Result<()> {
        let lock = self.lock(project_id);
        let _g = lock.lock().await;
        let mirror = self.mirror_path(project_id);
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

    pub async fn remove(&self, project_id: i64, dest: &Path) {
        let lock = self.lock(project_id);
        let _g = lock.lock().await;
        let mirror = self.mirror_path(project_id);
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
