//! `moloko runner`: the daemon inside a cloud runner VM. It joins the broker's warm pool, proves
//! what it is to the customer's server, runs exactly one job, and exits so the VM is destroyed.

use super::{Control, FromRunner, JobSpec, ToRunner, attest, channel, dial, recv_text, ws_url};
use crate::executor::{Executor, StepResult};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

pub struct Options {
    pub broker: String,
    pub pool_token: String,
    pub size: String,
    /// No hardware attestation. Customer servers refuse this unless they run with --cloud-insecure-dev.
    pub dev: bool,
    /// AMD product name for fetching the VCEK (Milan, Genoa).
    pub amd_product: String,
    /// VCEK certificate (DER) to use instead of fetching it from AMD.
    pub vcek: Option<PathBuf>,
    pub workdir: PathBuf,
}

/// Run one job, then return. The VM image runs this as its only service and powers off after.
pub async fn run(opts: Options) -> Result<()> {
    // Fetch the VCEK before joining the pool: it needs a report for the chip id and TCB, and
    // AMD's service can be slow. The key in this report is irrelevant, so bind zeros.
    let vcek = match (opts.dev, &opts.vcek) {
        (true, _) => Vec::new(),
        (false, Some(p)) => std::fs::read(p).with_context(|| format!("read {}", p.display()))?,
        (false, None) => {
            let (_, boot) = attest::tsm_report(&[0; 64])?;
            attest::fetch_vcek(&opts.amd_product, &boot)?
        }
    };
    let secret = x25519_dalek::EphemeralSecret::random();
    let runner_pub = x25519_dalek::PublicKey::from(&secret);

    let mut ws = dial(&ws_url(&opts.broker, &format!("/v1/pool?size={}", opts.size)), &opts.pool_token).await?;
    tracing::info!(size = opts.size, "in the warm pool, waiting for a lease");
    let Control::Leased { lease } = serde_json::from_str(&recv_text(&mut ws).await?)? else {
        bail!("expected a lease from the broker");
    };
    tracing::info!(lease, "leased");

    let Control::Hello { server_pub, nonce } = serde_json::from_str(&recv_text(&mut ws).await?)? else {
        bail!("expected hello from the customer's server");
    };
    let server_pub: [u8; 32] = hex::decode(server_pub)?.try_into().map_err(|_| anyhow!("bad server key"))?;
    let nonce: [u8; 32] = hex::decode(nonce)?.try_into().map_err(|_| anyhow!("bad nonce"))?;
    let data = attest::binding(runner_pub.as_bytes(), &server_pub, &nonce);
    let evidence = if opts.dev {
        attest::Evidence::Dev
    } else {
        let (provider, report) = attest::tsm_report(&data)?;
        if provider != "sev_guest" {
            bail!("attestation provider {provider:?} is not supported yet (SEV-SNP only)");
        }
        attest::Evidence::SevSnp { report: hex::encode(report), vcek: hex::encode(&vcek) }
    };
    let text = serde_json::to_string(&Control::Evidence { runner_pub: hex::encode(runner_pub.as_bytes()), evidence })?;
    ws.send(Message::Text(text.clone().into())).await?;
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(server_pub));
    let mut sealer = channel::Sealer::new(shared.as_bytes(), &nonce, text.as_bytes(), channel::Side::Runner);

    // Receive the bundles and the job.
    let job_dir = opts.workdir.join(format!("job-{lease}"));
    tokio::fs::create_dir_all(&job_dir).await?;
    let mut bundles: Vec<tokio::fs::File> = Vec::new();
    let job = loop {
        let frame = match ws.next().await {
            Some(Ok(Message::Binary(b))) => b,
            Some(Ok(Message::Close(_))) | None => {
                // Usually the server refused our attestation. Nothing to clean up but the dir.
                tracing::warn!(lease, "server left before sending a job");
                let _ = tokio::fs::remove_dir_all(&job_dir).await;
                return Ok(());
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        };
        match serde_json::from_slice::<ToRunner>(&sealer.open(&frame)?)? {
            ToRunner::Chunk { repo, data } => {
                while bundles.len() <= repo {
                    let p = job_dir.join(format!(".bundle-{}", bundles.len()));
                    bundles.push(tokio::fs::File::create(p).await?);
                }
                let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                bundles[repo].write_all(&bytes).await?;
            }
            ToRunner::Job(job) => break job,
        }
    };
    for f in &mut bundles {
        f.flush().await?;
    }
    drop(bundles);

    // Run it, streaming lines back. A closed connection means nobody is watching: stop.
    let (tx, mut rx) = mpsc::unbounded_channel::<FromRunner>();
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(execute(job, job_dir.clone(), tx, cancel.clone()));
    let (mut sink, mut stream) = ws.split();
    let watch = async {
        while let Some(Ok(m)) = stream.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    };
    let pump = async {
        while let Some(msg) = rx.recv().await {
            let done = matches!(msg, FromRunner::Done { .. });
            let frame = sealer.seal(&serde_json::to_vec(&msg).unwrap());
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                return;
            }
            if done {
                return;
            }
        }
    };
    tokio::select! {
        _ = pump => {}
        _ = watch => cancel.cancel(),
    }
    cancel.cancel();
    let _ = worker.await;
    let _ = sink.close().await;
    let _ = tokio::fs::remove_dir_all(&job_dir).await;
    tracing::info!(lease, "job finished");
    Ok(())
}

async fn execute(job: JobSpec, job_dir: PathBuf, tx: mpsc::UnboundedSender<FromRunner>, cancel: CancellationToken) {
    let line = |l: &str| {
        let _ = tx.send(FromRunner::Line { line: l.to_string() });
    };
    let (status, code) = match run_steps(&job, &job_dir, &line, &cancel).await {
        Ok(r) => r,
        Err(e) => {
            line(&format!("✗ {e:#}"));
            ("failed", None)
        }
    };
    let _ = tx.send(FromRunner::Done { status: status.into(), code });
}

async fn run_steps(
    job: &JobSpec,
    job_dir: &Path,
    log: &(dyn Fn(&str) + Sync),
    cancel: &CancellationToken,
) -> Result<(&'static str, Option<i32>)> {
    for (i, (name, sha)) in job.repos.iter().enumerate() {
        let dest = job_dir.join(name);
        let bundle = job_dir.join(format!(".bundle-{i}"));
        git(&["init", "--quiet", path(&dest)], job_dir).await?;
        // The bundle's only ref is a temporary one outside refs/heads, so fetch it explicitly.
        git(&["fetch", "--quiet", path(&bundle), "+refs/*:refs/bundle/*"], &dest).await?;
        git(&["checkout", "--quiet", "--detach", sha], &dest).await?;
        let _ = tokio::fs::remove_file(&bundle).await;
    }
    let primary = &job.repos.first().ok_or_else(|| anyhow!("job has no repository"))?.0;
    let ws = job_dir.join(primary);
    let exec = match &job.image {
        None => Executor::Host,
        Some(image) => {
            Executor::docker(image, "moloko-cloud-job", job_dir, &format!("/workspace/{primary}"), &job.env, log).await?
        }
    };
    let mut result = ("success", Some(0));
    for (label, script) in &job.steps {
        log(&format!("▶ {label}"));
        match exec.run_step(script, &ws, &job.env, log, cancel).await {
            Ok(StepResult::Exited(0)) => {}
            Ok(StepResult::Exited(c)) => {
                log(&format!("✗ step exited with code {c}"));
                result = ("failed", Some(c));
                break;
            }
            Ok(StepResult::Cancelled) => {
                result = ("cancelled", None);
                break;
            }
            Err(e) => {
                log(&format!("✗ {e:#}"));
                result = ("failed", None);
                break;
            }
        }
    }
    exec.shutdown().await;
    Ok(result)
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap_or_default()
}

async fn git(args: &[&str], cwd: &Path) -> Result<()> {
    let out = tokio::process::Command::new("git").args(args).current_dir(cwd).output().await?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
