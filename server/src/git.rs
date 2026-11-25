//! Bare git repositories, one per moloko repo: hosted, or mirrored from upstream.
//! Jobs check out via `git worktree`, so a checkout costs a few file writes instead of a clone.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::process::Command;

pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

pub fn is_zero(sha: &str) -> bool {
    sha.bytes().all(|b| b == b'0')
}

pub(crate) async fn git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    git_env(args, cwd, &[]).await
}

/// Run git with extra environment variables (e.g. credentials passed as `GIT_CONFIG_*`,
/// which unlike command-line arguments don't show up in `ps`).
pub(crate) async fn git_env(
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(String, String)],
) -> Result<String> {
    let out = git_output(args, cwd, env, None).await?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run git and return its raw output, whatever the exit status. `stdin` is written then closed.
pub(crate) async fn git_output(
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(String, String)],
    stdin: Option<&[u8]>,
) -> Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().context("failed to run git")?;
    if let Some(input) = stdin {
        let mut w = child.stdin.take().unwrap();
        w.write_all(input).await?;
        drop(w);
    }
    Ok(child.wait_with_output().await?)
}

/// Commit identity for commits moloko writes itself (merges, AI agent changes).
pub fn identity_env(name: &str, email: &str) -> Vec<(String, String)> {
    vec![
        ("GIT_AUTHOR_NAME".into(), name.into()),
        ("GIT_AUTHOR_EMAIL".into(), email.into()),
        ("GIT_COMMITTER_NAME".into(), name.into()),
        ("GIT_COMMITTER_EMAIL".into(), email.into()),
    ]
}

/// Bare repositories, one per moloko repo. Jobs check out via `git worktree`.
pub struct GitCache {
    root: PathBuf,
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
}

impl GitCache {
    pub fn new(root: &Path) -> Self {
        GitCache {
            root: root.to_path_buf(),
            locks: Mutex::default(),
        }
    }

    /// Serialises worktree and ref changes per repo.
    pub fn lock(&self, repo_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(repo_id)
            .or_default()
            .clone()
    }

    pub fn path(&self, repo_id: i64) -> PathBuf {
        self.root.join(format!("{repo_id}.git"))
    }

    /// Create the bare repo on first use.
    pub async fn ensure(&self, repo_id: i64, default_branch: &str) -> Result<PathBuf> {
        let path = self.path(repo_id);
        if !path.join("HEAD").exists() {
            let tmp = self.root.join(format!("{repo_id}.git.tmp"));
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            git(
                &[
                    "init",
                    "--bare",
                    "--quiet",
                    &format!("--initial-branch={default_branch}"),
                    tmp.to_str().unwrap(),
                ],
                None,
            )
            .await?;
            // Unreachable objects from rejected pushes are cleaned up by gc after two weeks.
            git(&["config", "gc.auto", "0"], Some(&tmp)).await?;
            tokio::fs::rename(&tmp, &path).await?;
        }
        Ok(path)
    }

    pub async fn resolve(&self, repo: &Path, git_ref: &str) -> Result<String> {
        for candidate in [
            git_ref.to_string(),
            format!("refs/heads/{git_ref}"),
            format!("refs/tags/{git_ref}"),
        ] {
            let spec = format!("{candidate}^{{commit}}");
            if let Ok(sha) = git(&["rev-parse", "--verify", "--quiet", &spec], Some(repo)).await {
                return Ok(sha);
            }
        }
        bail!("ref {git_ref:?} not found")
    }

    pub async fn read_file(&self, repo: &Path, sha: &str, path: &str) -> Result<String> {
        git(&["show", &format!("{sha}:{path}")], Some(repo)).await
    }

    /// Branch and tag refs with their commit shas.
    pub async fn refs(&self, repo: &Path) -> Result<HashMap<String, String>> {
        let out = git(
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/tags",
            ],
            Some(repo),
        )
        .await?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, s)| (r.to_string(), s.to_string()))
            .collect())
    }

    pub async fn is_ancestor(&self, repo: &Path, ancestor: &str, of: &str) -> bool {
        git_output(
            &["merge-base", "--is-ancestor", ancestor, of],
            Some(repo),
            &[],
            None,
        )
        .await
        .is_ok_and(|o| o.status.success())
    }

    pub async fn checkout(&self, repo_id: i64, sha: &str, dest: &Path) -> Result<()> {
        let lock = self.lock(repo_id);
        let _g = lock.lock().await;
        let repo = self.path(repo_id);
        git(
            &[
                "worktree",
                "add",
                "--detach",
                "--force",
                dest.to_str().unwrap(),
                sha,
            ],
            Some(&repo),
        )
        .await?;
        Ok(())
    }

    pub async fn remove(&self, repo_id: i64, dest: &Path) {
        let lock = self.lock(repo_id);
        let _g = lock.lock().await;
        let repo = self.path(repo_id);
        if git(
            &["worktree", "remove", "--force", dest.to_str().unwrap()],
            Some(&repo),
        )
        .await
        .is_err()
        {
            let _ = tokio::fs::remove_dir_all(dest).await;
            let _ = git(&["worktree", "prune"], Some(&repo)).await;
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
        let mirror = cache.ensure(1, "main").await.unwrap();
        git(&["fetch", "--quiet", repo.to_str().unwrap(), "+refs/heads/*:refs/heads/*"], Some(&mirror)).await.unwrap();
        let sha = cache.resolve(&mirror, "main").await.unwrap();
        assert_eq!(cache.refs(&mirror).await.unwrap()["refs/heads/main"], sha);
        assert!(cache.is_ancestor(&mirror, &sha, &sha).await);
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
