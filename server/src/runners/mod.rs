//! moloko runners: cloud build machines that rent out by the minute and can't see what they build.
//!
//! Three parties. The customer's moloko server holds the source and secrets. The broker (moloko
//! cloud) hands out leases and relays bytes. The runner is a single-use confidential VM. The
//! server only ships a job after it has verified the runner's hardware attestation itself, over
//! a channel whose keys the broker never sees. See docs/runners.md.

pub mod attest;
pub mod broker;
pub mod channel;
pub mod runner;

use crate::db::Status;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

pub const SIZES: &[(&str, f64)] = &[("s", 0.5), ("m", 1.0), ("l", 2.0), ("xl", 4.0)];

/// Included minutes are counted in m-minutes: an `l` minute is two, an `s` minute half.
pub fn weight(size: &str) -> Option<f64> {
    SIZES.iter().find(|(s, _)| *s == size).map(|(_, w)| *w)
}

/// Bundles are sent in pieces so a large repository doesn't hit WebSocket message limits.
const CHUNK: usize = 1 << 20;

/// Plain-text frames: broker <-> either side, and the handshake before keys exist.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Leased { lease: String },
    Refused { status: u16, reason: String },
    Hello { server_pub: String, nonce: String },
    Evidence { runner_pub: String, evidence: attest::Evidence },
}

/// Sealed frames, server -> runner.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToRunner {
    Chunk { repo: usize, data: String },
    Job(JobSpec),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobSpec {
    /// Repositories in the order their chunks were sent: name (directory) and commit.
    pub repos: Vec<(String, String)>,
    pub env: Vec<(String, String)>,
    /// (label, script)
    pub steps: Vec<(String, String)>,
    pub image: Option<String>,
}

/// Sealed frames, runner -> server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromRunner {
    Line { line: String },
    Done { status: String, code: Option<i32> },
}

/// How this moloko server reaches moloko runners. From `--cloud-*` flags or `MOLOKO_CLOUD_*`.
#[derive(Debug, Clone)]
pub struct CloudConfig {
    pub broker: String,
    pub key: String,
    pub policy: attest::Policy,
}

/// What the dashboard shows on a cloud job, and what a bill can be checked against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerInfo {
    pub size: String,
    pub lease: String,
    pub attestation: attest::Verified,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl RunnerInfo {
    /// Billed m-minutes: whole minutes from assignment to the end, times the size weight.
    pub fn m_minutes(&self) -> f64 {
        let ms = self.ended_at.unwrap_or_else(crate::db::now_ms) - self.started_at;
        let minutes = ((ms.max(0) + 59_999) / 60_000) as f64;
        minutes * weight(&self.size).unwrap_or(1.0)
    }
}

/// A job ready to ship: bundle files are already on disk.
pub struct CloudJob {
    pub repos: Vec<(String, String, PathBuf)>,
    pub env: Vec<(String, String)>,
    pub steps: Vec<(String, String)>,
    pub image: Option<String>,
}

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// A leased runner whose attestation passed. Nothing of the customer's has been sent yet.
pub struct Session {
    ws: Ws,
    sealer: channel::Sealer,
    pub info: RunnerInfo,
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .expect("read /dev/urandom");
    buf
}

pub(crate) fn ws_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = if let Some(r) = base.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = base.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        base.to_string()
    };
    format!("{base}{path}")
}

pub(crate) async fn dial(url: &str, token: &str) -> Result<Ws> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut req = url.into_client_request().context("invalid broker URL")?;
    req.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(req).await.with_context(|| format!("connect {url}"))?;
    Ok(ws)
}

async fn recv_text(ws: &mut Ws) -> Result<String> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(t.to_string()),
            Some(Ok(Message::Close(_))) | None => bail!("connection closed"),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

/// Lease a runner and verify it. Any error here means nothing was sent, so falling back to a
/// local executor is safe.
pub async fn lease(cfg: &CloudConfig, size: &str, log: &(dyn Fn(&str) + Sync)) -> Result<Session> {
    let mut ws = dial(&ws_url(&cfg.broker, &format!("/v1/lease?size={size}")), &cfg.key).await?;
    let lease = match serde_json::from_str::<Control>(&recv_text(&mut ws).await?)? {
        Control::Leased { lease } => lease,
        Control::Refused { status, reason } => bail!("broker refused the lease ({status}): {reason}"),
        other => bail!("unexpected message from broker: {other:?}"),
    };
    let started_at = crate::db::now_ms();
    log(&format!("▶ leased a cloud runner, size {size} (lease {lease})"));

    let secret = x25519_dalek::EphemeralSecret::random();
    let server_pub = x25519_dalek::PublicKey::from(&secret);
    let nonce: [u8; 32] = random();
    let hello = Control::Hello { server_pub: hex::encode(server_pub.as_bytes()), nonce: hex::encode(nonce) };
    ws.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;

    let text = recv_text(&mut ws).await?;
    let Control::Evidence { runner_pub, evidence } = serde_json::from_str(&text)? else {
        bail!("runner sent no attestation evidence");
    };
    let runner_pub: [u8; 32] = hex::decode(&runner_pub)?
        .try_into()
        .map_err(|_| anyhow!("runner key is not 32 bytes"))?;
    let expect = attest::binding(&runner_pub, server_pub.as_bytes(), &nonce);
    let verified = attest::verify(&cfg.policy, &evidence, &expect).context("runner attestation failed")?;
    log(&format!(
        "▶ runner attested: {}{}",
        verified.kind,
        if verified.measurement.is_empty() { String::new() } else { format!(", image {}…", &verified.measurement[..16]) }
    ));
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(runner_pub));
    let sealer = channel::Sealer::new(shared.as_bytes(), &nonce, text.as_bytes(), channel::Side::Server);
    Ok(Session {
        ws,
        sealer,
        info: RunnerInfo { size: size.into(), lease, attestation: verified, started_at, ended_at: None },
    })
}

impl Session {
    async fn send(&mut self, msg: &ToRunner) -> Result<()> {
        let frame = self.sealer.seal(&serde_json::to_vec(msg)?);
        self.ws.send(Message::Binary(frame.into())).await?;
        Ok(())
    }

    /// Ship the job, stream its log, and return how it ended. Closing the connection ends the
    /// runner, so cancelling just drops it.
    pub async fn run(
        mut self,
        job: CloudJob,
        log: &(dyn Fn(&str) + Sync),
        cancel: &CancellationToken,
    ) -> (Status, Option<i32>, RunnerInfo) {
        let r = tokio::select! {
            r = self.drive(job, log) => r,
            _ = cancel.cancelled() => Ok((Status::Cancelled, None)),
        };
        let _ = self.ws.close(None).await;
        self.info.ended_at = Some(crate::db::now_ms());
        match r {
            Ok((s, c)) => (s, c, self.info),
            Err(e) => {
                log(&format!("✗ cloud runner: {e:#}"));
                (Status::Failed, None, self.info)
            }
        }
    }

    async fn drive(&mut self, job: CloudJob, log: &(dyn Fn(&str) + Sync)) -> Result<(Status, Option<i32>)> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut repos = Vec::new();
        for (i, (name, sha, path)) in job.repos.iter().enumerate() {
            let bytes = tokio::fs::read(path).await.with_context(|| format!("read bundle {}", path.display()))?;
            for piece in bytes.chunks(CHUNK) {
                self.send(&ToRunner::Chunk { repo: i, data: b64.encode(piece) }).await?;
            }
            repos.push((name.clone(), sha.clone()));
        }
        self.send(&ToRunner::Job(JobSpec { repos, env: job.env, steps: job.steps, image: job.image }))
            .await?;
        loop {
            let frame = match self.ws.next().await {
                Some(Ok(Message::Binary(b))) => b,
                Some(Ok(Message::Close(_))) | None => bail!("runner disconnected"),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            };
            match serde_json::from_slice::<FromRunner>(&self.sealer.open(&frame)?)? {
                FromRunner::Line { line } => log(&line),
                FromRunner::Done { status, code } => {
                    let s = match status.as_str() {
                        "success" => Status::Success,
                        "cancelled" => Status::Cancelled,
                        _ => Status::Failed,
                    };
                    return Ok((s, code));
                }
            }
        }
    }
}
