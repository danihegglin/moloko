//! Runs job steps directly on the host, or inside one warm Docker container per job
//! (started once, then `docker exec` per step).

use crate::logs::JobLog;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub enum Executor {
    Host,
    Docker { container: String },
}

pub enum StepResult {
    Exited(i32),
    Cancelled,
}

impl Executor {
    pub async fn docker(
        image: &str,
        name: &str,
        workspace: &Path,
        env: &[(String, String)],
        log: &JobLog,
    ) -> Result<Self> {
        log.line(&format!("▶ docker: starting {image}"));
        let mut cmd = Command::new("docker");
        cmd.args([
            "run",
            "-d",
            "--rm",
            "--name",
            name,
            "--entrypoint",
            "sh",
            "-w",
            "/workspace",
            "-v",
        ])
        .arg(format!("{}:/workspace", workspace.display()));
        // `-e NAME` takes the value from docker's own environment, keeping it out of argv.
        for (k, v) in env {
            cmd.env(k, v).arg("-e").arg(k);
        }
        cmd.args([image, "-c", "trap 'exit 0' TERM; sleep infinity & wait"]);
        let out = cmd.output().await.context("failed to run docker")?;
        if !out.status.success() {
            bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Executor::Docker {
            container: name.to_string(),
        })
    }

    pub async fn run_step(
        &self,
        script: &str,
        workspace: &Path,
        env: &[(String, String)],
        log: &(dyn Fn(&str) + Sync),
        cancel: &CancellationToken,
    ) -> Result<StepResult> {
        let cmd = match self {
            Executor::Host => {
                let mut c = Command::new("sh");
                c.args(["-e", "-c", script])
                    .current_dir(workspace)
                    .envs(env.iter().cloned());
                c
            }
            Executor::Docker { container } => {
                let mut c = Command::new("docker");
                c.args(["exec", container, "sh", "-e", "-c", script]);
                c
            }
        };
        stream(cmd, log, cancel).await
    }

    pub async fn shutdown(&self) {
        if let Executor::Docker { container } = self {
            let _ = Command::new("docker")
                .args(["rm", "-f", container])
                .output()
                .await;
        }
    }
}

/// Run `cmd`, sending each stdout/stderr line to `log`. On cancellation the whole process group is killed.
pub async fn stream(
    mut cmd: Command,
    log: &(dyn Fn(&str) + Sync),
    cancel: &CancellationToken,
) -> Result<StepResult> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("failed to spawn step")?;
    let pid = child.id();
    let out = pump(child.stdout.take().unwrap(), log);
    let err = pump(child.stderr.take().unwrap(), log);
    let pumps = async { tokio::join!(out, err) };

    tokio::select! {
        (status, _) = async { tokio::join!(child.wait(), pumps) } => {
            Ok(StepResult::Exited(status?.code().unwrap_or(-1)))
        }
        _ = cancel.cancelled() => {
            if let Some(pid) = pid {
                // Kill the whole process group so grandchildren (e.g. a JVM) die too.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.wait().await;
            Ok(StepResult::Cancelled)
        }
    }
}

async fn pump(stream: impl AsyncRead + Unpin, log: &(dyn Fn(&str) + Sync)) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log(&line);
    }
}
