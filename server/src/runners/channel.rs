//! The end-to-end channel between the customer's moloko server and a runner. The relay in
//! between forwards these frames but holds no key.
//!
//! Keys: X25519 between the server's per-lease key and the runner's boot-time key, expanded
//! with HKDF-SHA256 over the lease nonce and the attestation evidence. Frames: ChaCha20-Poly1305
//! with a per-direction counter as nonce, so a dropped, replayed or reordered frame fails.

use anyhow::{Result, anyhow};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

pub struct Sealer {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    sent: u64,
    received: u64,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Server,
    Runner,
}

impl Sealer {
    pub fn new(shared: &[u8; 32], nonce: &[u8; 32], evidence: &[u8], side: Side) -> Self {
        let mut info = b"moloko-runner-v1 ".to_vec();
        info.extend(Sha256::digest(evidence));
        let mut okm = [0u8; 64];
        Hkdf::<Sha256>::new(Some(nonce), shared)
            .expand(&info, &mut okm)
            .expect("64 bytes is a valid HKDF length");
        let to_runner = ChaCha20Poly1305::new_from_slice(&okm[..32]).expect("32-byte key");
        let to_server = ChaCha20Poly1305::new_from_slice(&okm[32..]).expect("32-byte key");
        let (send, recv) = match side {
            Side::Server => (to_runner, to_server),
            Side::Runner => (to_server, to_runner),
        };
        Sealer { send, recv, sent: 0, received: 0 }
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let n = counter_nonce(self.sent);
        self.sent += 1;
        self.send.encrypt(&n, plain).expect("encryption cannot fail")
    }

    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>> {
        let n = counter_nonce(self.received);
        let plain = self
            .recv
            .decrypt(&n, sealed)
            .map_err(|_| anyhow!("channel frame failed authentication"))?;
        self.received += 1;
        Ok(plain)
    }
}

fn counter_nonce(c: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&c.to_le_bytes());
    n.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Sealer, Sealer) {
        let (s, n) = ([1u8; 32], [2u8; 32]);
        (Sealer::new(&s, &n, b"ev", Side::Server), Sealer::new(&s, &n, b"ev", Side::Runner))
    }

    #[test]
    fn round_trip_both_ways() {
        let (mut srv, mut run) = pair();
        assert_eq!(run.open(&srv.seal(b"job")).unwrap(), b"job");
        assert_eq!(srv.open(&run.seal(b"log")).unwrap(), b"log");
        assert_eq!(run.open(&srv.seal(b"two")).unwrap(), b"two");
    }

    #[test]
    fn replay_and_tamper_fail() {
        let (mut srv, mut run) = pair();
        let f = srv.seal(b"job");
        run.open(&f).unwrap();
        assert!(run.open(&f).is_err(), "replayed frame accepted");
        let mut g = srv.seal(b"x");
        g[0] ^= 1;
        assert!(run.open(&g).is_err(), "tampered frame accepted");
    }

    #[test]
    fn different_evidence_gives_different_keys() {
        let (s, n) = ([1u8; 32], [2u8; 32]);
        let mut a = Sealer::new(&s, &n, b"ev-a", Side::Server);
        let mut b = Sealer::new(&s, &n, b"ev-b", Side::Runner);
        assert!(b.open(&a.seal(b"job")).is_err());
    }
}
