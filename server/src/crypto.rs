//! Encrypts secrets at rest (mirror credentials, AI agent secrets) with a server key.
//! The key comes from `MOLOKO_SECRET_KEY` (64 hex chars) or `<data>/secret.key`, created on first start.

use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::path::Path;

pub struct Secrets {
    cipher: ChaCha20Poly1305,
}

fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .expect("read /dev/urandom");
    buf
}

impl Secrets {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let hex_key = match std::env::var("MOLOKO_SECRET_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                let file = data_dir.join("secret.key");
                match std::fs::read_to_string(&file) {
                    Ok(k) => k,
                    Err(_) => {
                        let k = hex::encode(random::<32>());
                        std::fs::write(&file, &k)?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
                        k
                    }
                }
            }
        };
        let bytes = hex::decode(hex_key.trim()).context("secret key must be hex")?;
        if bytes.len() != 32 {
            bail!("secret key must be 32 bytes (64 hex characters)");
        }
        let key = Key::try_from(bytes.as_slice()).map_err(|_| anyhow!("bad key length"))?;
        Ok(Secrets {
            cipher: ChaCha20Poly1305::new(&key),
        })
    }

    /// nonce || ciphertext, hex encoded.
    pub fn seal(&self, plain: &str) -> String {
        let nonce_bytes = random::<12>();
        let nonce = Nonce::try_from(nonce_bytes.as_slice()).expect("12-byte nonce");
        let ct = self
            .cipher
            .encrypt(&nonce, plain.as_bytes())
            .expect("encryption cannot fail");
        let mut out = nonce_bytes.to_vec();
        out.extend(ct);
        hex::encode(out)
    }

    pub fn open(&self, sealed: &str) -> Result<String> {
        let raw = hex::decode(sealed)?;
        if raw.len() < 12 {
            bail!("sealed value too short");
        }
        let nonce = Nonce::try_from(&raw[..12]).map_err(|_| anyhow!("bad nonce"))?;
        let plain = self
            .cipher
            .decrypt(&nonce, &raw[12..])
            .map_err(|_| anyhow!("cannot decrypt secret: wrong MOLOKO_SECRET_KEY?"))?;
        Ok(String::from_utf8(plain)?)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn seal_open() {
        let dir = tempfile::tempdir().unwrap();
        let s = super::Secrets::load(dir.path()).unwrap();
        let sealed = s.seal("ghp_abc");
        assert_ne!(sealed, "ghp_abc");
        assert_eq!(s.open(&sealed).unwrap(), "ghp_abc");
        // Same key file, same result.
        let s2 = super::Secrets::load(dir.path()).unwrap();
        assert_eq!(s2.open(&sealed).unwrap(), "ghp_abc");
    }
}
