//! Content-addressed directory cache. Restores use copy-on-write clones
//! (APFS clonefile on macOS, reflinks on btrfs/xfs) so large caches restore near-instantly.

use crate::config::CacheSpec;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::Command;

pub fn key(job: &str, spec: &CacheSpec, workspace: &Path) -> String {
    let mut h = Sha256::new();
    h.update(job.as_bytes());
    h.update([0]);
    h.update(spec.path.as_bytes());
    for f in &spec.key_files {
        h.update([0]);
        h.update(f.as_bytes());
        h.update([0]);
        match std::fs::read(workspace.join(f)) {
            Ok(bytes) => h.update(&bytes),
            Err(_) => h.update(b"<missing>"),
        }
    }
    hex::encode(h.finalize())
}

/// Copy a directory tree, preferring copy-on-write.
async fn clone_tree(src: &Path, dst: &Path) -> bool {
    let fast: &[&str] = if cfg!(target_os = "macos") {
        &["-c", "-R"]
    } else {
        &["-a", "--reflink=auto"]
    };
    for flags in [fast, &["-R"][..]] {
        let ok = Command::new("cp")
            .args(flags)
            .arg(src)
            .arg(dst)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        let _ = tokio::fs::remove_dir_all(dst).await;
    }
    false
}

pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new(root: &Path) -> Self {
        Cache {
            root: root.to_path_buf(),
        }
    }

    /// Returns true on a cache hit.
    pub async fn restore(&self, key: &str, dst: &Path) -> bool {
        let src = self.root.join(key);
        if !src.exists() || dst.exists() {
            return false;
        }
        if let Some(parent) = dst.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        clone_tree(&src, dst).await
    }

    /// Store `src` under `key` unless already present. Writes to a temp dir and renames atomically.
    pub async fn save(&self, key: &str, src: &Path) -> bool {
        let dst = self.root.join(key);
        if dst.exists() || !src.is_dir() {
            return false;
        }
        // Unique per save so concurrent jobs writing the same key don't share a temp dir.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .root
            .join(format!("{key}.tmp-{}-{seq}", std::process::id()));
        if !clone_tree(src, &tmp).await {
            return false;
        }
        if tokio::fs::rename(&tmp, &dst).await.is_err() {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_then_restore() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("target/deep")).unwrap();
        std::fs::write(ws.join("Cargo.lock"), "v1").unwrap();
        std::fs::write(ws.join("target/deep/x"), "built").unwrap();
        let spec = CacheSpec {
            path: "target".into(),
            key_files: vec!["Cargo.lock".into()],
        };
        let k = key("build", &spec, &ws);
        let cache = Cache::new(&dir.path().join("cache"));
        std::fs::create_dir_all(dir.path().join("cache")).unwrap();
        assert!(cache.save(&k, &ws.join("target")).await);
        assert!(
            !cache.save(&k, &ws.join("target")).await,
            "second save is a no-op"
        );

        let ws2 = dir.path().join("ws2");
        std::fs::create_dir_all(&ws2).unwrap();
        std::fs::write(ws2.join("Cargo.lock"), "v1").unwrap();
        assert_eq!(key("build", &spec, &ws2), k);
        assert!(cache.restore(&k, &ws2.join("target")).await);
        assert_eq!(
            std::fs::read_to_string(ws2.join("target/deep/x")).unwrap(),
            "built"
        );

        std::fs::write(ws2.join("Cargo.lock"), "v2").unwrap();
        assert_ne!(key("build", &spec, &ws2), k);
    }
}
