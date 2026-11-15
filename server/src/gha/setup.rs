//! What runners fetch on first use (runner images, the Windows SDK), so it can also be
//! downloaded ahead of time from the dashboard's settings page.

use super::{DEFAULT_IMAGES, Shared, WINDOWS_CROSS_IMAGE, builtin_dockerfile};
use crate::executor::{StepResult, stream};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const WINDOWS_SDK: &str = "windows-sdk";

#[derive(Serialize)]
pub struct Download {
    pub id: String,
    pub name: String,
    pub detail: String,
    /// `ready`, `missing` or `downloading`.
    pub state: &'static str,
    /// The latest output line while downloading, or the error of the last attempt.
    pub message: Option<String>,
}

/// In-flight and failed downloads: id → (running, latest line or error).
pub type Progress = std::sync::Mutex<std::collections::HashMap<String, (bool, Option<String>)>>;

async fn image_exists(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// Build or pull `image` unless it is already there. `scratch` holds the build context.
pub async fn ensure_image(
    shared: &Shared,
    image: &str,
    scratch: &Path,
    log: &(dyn Fn(&str) + Sync),
    cancel: &CancellationToken,
) -> Result<()> {
    if image_exists(image).await {
        return Ok(());
    }
    let mut cmd = Command::new("docker");
    if let Some(dockerfile) = builtin_dockerfile(image) {
        // Serialise builds: concurrent jobs would otherwise build the same image at once.
        let lock = shared.lock(&format!("image:{image}"));
        let _g = lock.lock().await;
        if image_exists(image).await {
            return Ok(());
        }
        log(&format!("▶ building runner image {image} (first use, takes a few minutes)"));
        let ctx = scratch.join("_runner_image");
        tokio::fs::create_dir_all(&ctx).await?;
        tokio::fs::write(ctx.join("Dockerfile"), dockerfile).await?;
        cmd.args(["build", "-q", "-t", image]).arg(&ctx);
        return match stream(cmd, log, cancel).await? {
            StepResult::Exited(0) => Ok(()),
            StepResult::Exited(c) => bail!("building {image} failed with exit code {c}"),
            StepResult::Cancelled => bail!("cancelled"),
        };
    }
    log(&format!("▶ pulling {image} (first use can take a while)"));
    cmd.args(["pull", "-q", image]);
    match stream(cmd, log, cancel).await? {
        StepResult::Exited(0) => Ok(()),
        StepResult::Exited(c) => bail!("docker pull {image} failed with exit code {c}"),
        StepResult::Cancelled => bail!("cancelled"),
    }
}

/// (id, name, detail) of everything that can be downloaded. An image's id is its name.
fn items(shared: &Shared) -> Vec<(String, String, String)> {
    // Runner image → the `runs-on` labels that use it; `--gha-image` overrides the defaults.
    let mut images: Vec<(String, Vec<String>)> = Vec::new();
    let overrides = shared.config.images.iter().map(|(l, i)| (l.as_str(), i.as_str()));
    let defaults = DEFAULT_IMAGES
        .iter()
        .copied()
        .filter(|(l, _)| !shared.config.images.iter().any(|(o, _)| o.eq_ignore_ascii_case(l)));
    for (label, image) in overrides.chain(defaults).filter(|(_, i)| *i != "host") {
        match images.iter_mut().find(|(i, _)| i == image) {
            Some((_, labels)) => labels.push(label.to_string()),
            None => images.push((image.to_string(), vec![label.to_string()])),
        }
    }
    let mut out: Vec<_> = images
        .into_iter()
        .map(|(image, labels)| (image.clone(), image, format!("Runner image for {}", labels.join(", "))))
        .collect();
    out.push((
        WINDOWS_CROSS_IMAGE.into(),
        WINDOWS_CROSS_IMAGE.into(),
        "Runner image for windows-* jobs: Linux with a Windows cross toolchain".into(),
    ));
    out.push((
        WINDOWS_SDK.into(),
        "Windows SDK and C runtime".into(),
        "Microsoft's headers and libraries, which Windows cross-compiles link against. About 2 GB".into(),
    ));
    out
}

pub async fn list(shared: &Shared) -> Vec<Download> {
    let mut out = Vec::new();
    for (id, name, detail) in items(shared) {
        let (running, message) = shared.downloads.lock().unwrap().get(&id).cloned().unwrap_or_default();
        let ready = if id == WINDOWS_SDK {
            shared.xwin_dir().join("xwin/DONE").exists()
        } else {
            image_exists(&id).await
        };
        let state = if running { "downloading" } else if ready { "ready" } else { "missing" };
        out.push(Download { id, name, detail, state, message });
    }
    out
}

/// Start downloading `id` in the background. Returns false when there is no such download.
pub fn start(engine: &Arc<crate::engine::Engine>, id: &str) -> bool {
    let shared = &engine.gha;
    if !items(shared).iter().any(|(i, ..)| i == id) {
        return false;
    }
    {
        let mut d = shared.downloads.lock().unwrap();
        if d.get(id).is_some_and(|(running, _)| *running) {
            return true;
        }
        d.insert(id.to_string(), (true, None));
    }
    let (engine, id) = (engine.clone(), id.to_string());
    tokio::spawn(async move {
        let shared = &engine.gha;
        let set = |running: bool, message: Option<String>| {
            shared.downloads.lock().unwrap().insert(id.clone(), (running, message));
        };
        let log = |l: &str| set(true, Some(l.to_string()));
        match download(shared, &id, &log).await {
            Ok(()) => set(false, None),
            Err(e) => set(false, Some(format!("{e:#}"))),
        }
    });
    true
}

async fn download(shared: &Shared, id: &str, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let cancel = CancellationToken::new();
    let scratch = shared.actions_dir.with_file_name("setup");
    if id != WINDOWS_SDK {
        return ensure_image(shared, id, &scratch, log, &cancel).await;
    }
    ensure_image(shared, WINDOWS_CROSS_IMAGE, &scratch, log, &cancel).await?;
    let xwin = shared.xwin_dir();
    tokio::fs::create_dir_all(&xwin).await?;
    let xwin = xwin.display().to_string();
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "-v", &format!("{xwin}:{xwin}"), "-e", &format!("XWIN_CACHE_DIR={xwin}")])
        .args([WINDOWS_CROSS_IMAGE, "cargo", "xwin", "cache", "xwin"]);
    match stream(cmd, log, &cancel).await.context("running docker")? {
        StepResult::Exited(0) => Ok(()),
        StepResult::Exited(c) => bail!("downloading the Windows SDK failed with exit code {c}"),
        StepResult::Cancelled => bail!("cancelled"),
    }
}
