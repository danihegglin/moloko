//! Secret vault: values are encrypted at rest with ChaCha20-Poly1305 and only decrypted
//! when a run needs them. The API never returns a stored value.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::path::Path;

pub const MAX_VALUE_BYTES: usize = 48 * 1024;

pub struct Vault {
    key: LessSafeKey,
    rng: SystemRandom,
}

/// Where a secret lives: a project (shared by its repos) or one repo.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Project(i64),
    Repo(i64),
}

impl Scope {
    pub fn key(self) -> String {
        match self {
            Scope::Project(id) => format!("p:{id}"),
            Scope::Repo(id) => format!("r:{id}"),
        }
    }
}

/// GitHub's rules: letters, digits and `_`, not starting with a digit. Stored uppercase.
pub fn normalize_name(name: &str) -> Result<String> {
    let n = name.trim().to_uppercase();
    let valid = !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("secret names may only contain letters, digits and _, and can't start with a digit");
    }
    Ok(n)
}

impl Vault {
    /// Use `MOLOKO_SECRETS_KEY` (base64, 32 bytes) if set, else `<data>/secrets.key`, created on first use.
    pub fn open(data_dir: &Path, env_key: Option<&str>) -> Result<Self> {
        let raw = match env_key.filter(|k| !k.trim().is_empty()) {
            Some(k) => B64.decode(k.trim()).context("MOLOKO_SECRETS_KEY must be base64")?,
            None => {
                let path = data_dir.join("secrets.key");
                match std::fs::read_to_string(&path) {
                    Ok(k) => B64.decode(k.trim()).context("secrets.key is corrupt")?,
                    Err(_) => {
                        let mut k = [0u8; 32];
                        SystemRandom::new().fill(&mut k).map_err(|_| anyhow!("no randomness"))?;
                        std::fs::write(&path, B64.encode(k))?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                        k.to_vec()
                    }
                }
            }
        };
        let key = UnboundKey::new(&CHACHA20_POLY1305, &raw).map_err(|_| anyhow!("the secrets key must be 32 bytes"))?;
        Ok(Vault { key: LessSafeKey::new(key), rng: SystemRandom::new() })
    }

    /// Returns (nonce, ciphertext). The scope and name are authenticated, so a value
    /// can't be moved to another secret in the database.
    pub fn seal(&self, scope: Scope, name: &str, value: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill(&mut nonce).map_err(|_| anyhow!("no randomness"))?;
        let mut buf = value.as_bytes().to_vec();
        let aad = format!("{}/{name}", scope.key());
        self.key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad.as_bytes()), &mut buf)
            .map_err(|_| anyhow!("encryption failed"))?;
        Ok((nonce.to_vec(), buf))
    }

    pub fn open_value(&self, scope_key: &str, name: &str, nonce: &[u8], sealed: &[u8]) -> Result<String> {
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| anyhow!("bad nonce"))?;
        let mut buf = sealed.to_vec();
        let aad = format!("{scope_key}/{name}");
        let plain = self
            .key
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad.as_bytes()), &mut buf)
            .map_err(|_| anyhow!("can't decrypt secret {name}: wrong secrets key?"))?;
        Ok(String::from_utf8(plain.to_vec())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::open(dir.path(), None).unwrap();
        let (n, c) = v.seal(Scope::Repo(1), "TOKEN", "s3cret\nline2").unwrap();
        assert!(!c.windows(6).any(|w| w == b"s3cret"));
        assert_eq!(v.open_value("r:1", "TOKEN", &n, &c).unwrap(), "s3cret\nline2");
        // Bound to scope and name.
        assert!(v.open_value("r:2", "TOKEN", &n, &c).is_err());
        assert!(v.open_value("r:1", "OTHER", &n, &c).is_err());
        // The key persists across restarts.
        let v2 = Vault::open(dir.path(), None).unwrap();
        assert_eq!(v2.open_value("r:1", "TOKEN", &n, &c).unwrap(), "s3cret\nline2");
        let mode = std::fs::metadata(dir.path().join("secrets.key")).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
    }

    #[test]
    fn names() {
        assert_eq!(normalize_name(" github_token ").unwrap(), "GITHUB_TOKEN");
        for bad in ["", "1A", "A-B", "A B"] {
            assert!(normalize_name(bad).is_err(), "{bad}");
        }
    }
}
