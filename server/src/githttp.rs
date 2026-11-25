//! Git over smart HTTP: `git clone http://host/git/<project>/<repo>.git`.
//!
//! Fetches are served by `git upload-pack`. Pushes are handled here instead of by
//! `git receive-pack`: moloko stores the pack with `git index-pack`, then sends the ref
//! changes through `Engine::apply_ref_updates`, which checks policy, writes through to
//! GitHub in two-way mode and updates all refs in one transaction.

use crate::api::AppState;
use crate::auth::{Actor, Role};
use crate::repos::{RefUpdate, Repo, Source};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use serde::Deserialize;
use std::io::Read;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/git/{project}/{repo}/info/refs", get(info_refs))
        .route("/git/{project}/{repo}/git-upload-pack", post(upload_pack))
        .route("/git/{project}/{repo}/git-receive-pack", post(receive_pack))
}

/// Encode one pkt-line.
pub fn pkt(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

const FLUSH: &[u8] = b"0000";

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"moloko\"")],
        "authentication required: use your username and a personal access token\n",
    )
        .into_response()
}

fn plain(status: StatusCode, msg: &str) -> Response {
    (status, format!("{msg}\n")).into_response()
}

/// Resolve `<project>/<repo>.git` and check access. Unknown repos and missing access look the same.
fn locate(
    s: &AppState,
    headers: &HeaderMap,
    project: &str,
    repo: &str,
    min: Role,
) -> Result<(Repo, Actor), Response> {
    let actor = match s.engine.db.authenticate(headers) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(unauthorized()),
        Err(e) => return Err(plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"))),
    };
    let name = repo.strip_suffix(".git").unwrap_or(repo);
    let found = s
        .engine
        .db
        .project_by_name(project)
        .ok()
        .flatten()
        .and_then(|p| s.engine.db.repo_by_name(p.id, name).ok().flatten());
    let Some(repo) = found else {
        return Err(plain(StatusCode::NOT_FOUND, "repository not found"));
    };
    match crate::api::need(s, &actor, repo.project_id, min) {
        Ok(_) => Ok((repo, actor)),
        Err(e) if e.0 == StatusCode::FORBIDDEN => Err(plain(StatusCode::FORBIDDEN, &e.1)),
        Err(_) => Err(plain(StatusCode::NOT_FOUND, "repository not found")),
    }
}

#[derive(Deserialize)]
struct InfoQuery {
    service: Option<String>,
}

fn protocol_env(headers: &HeaderMap) -> Option<String> {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric() || "=:._-".contains(c)))
        .map(str::to_string)
}

async fn info_refs(
    State(s): State<AppState>,
    Path((project, repo)): Path<(String, String)>,
    Query(q): Query<InfoQuery>,
    headers: HeaderMap,
) -> Response {
    let service = q.service.unwrap_or_default();
    let min = match service.as_str() {
        "git-upload-pack" => Role::Read,
        "git-receive-pack" => Role::Write,
        _ => return plain(StatusCode::FORBIDDEN, "only smart HTTP is supported"),
    };
    let (repo, _actor) = match locate(&s, &headers, &project, &repo, min) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let path = match s.engine.git.ensure(repo.id, &repo.default_branch).await {
        Ok(p) => p,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    };
    let mut body = Vec::new();
    if service == "git-upload-pack" {
        let proto = protocol_env(&headers);
        let v2 = proto.as_deref().is_some_and(|p| p.contains("version=2"));
        if !v2 {
            body.extend(pkt(b"# service=git-upload-pack\n"));
            body.extend(FLUSH);
        }
        let mut cmd = Command::new("git");
        cmd.args(["upload-pack", "--stateless-rpc", "--advertise-refs"])
            .arg(&path)
            .stdin(Stdio::null());
        if let Some(p) = proto {
            cmd.env("GIT_PROTOCOL", p);
        }
        match cmd.output().await {
            Ok(out) if out.status.success() => body.extend(out.stdout),
            Ok(out) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &String::from_utf8_lossy(&out.stderr)),
            Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    } else {
        body.extend(pkt(b"# service=git-receive-pack\n"));
        body.extend(FLUSH);
        let refs = match s.engine.git.refs(&path).await {
            Ok(r) => r,
            Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
        };
        let caps = "report-status delete-refs side-band-64k quiet atomic ofs-delta agent=moloko";
        let mut refs: Vec<_> = refs.into_iter().collect();
        refs.sort();
        if refs.is_empty() {
            body.extend(pkt(format!("{} capabilities^{{}}\0{caps}\n", crate::git::ZERO_SHA).as_bytes()));
        }
        for (i, (name, sha)) in refs.iter().enumerate() {
            let line = if i == 0 {
                format!("{sha} {name}\0{caps}\n")
            } else {
                format!("{sha} {name}\n")
            };
            body.extend(pkt(line.as_bytes()));
        }
        body.extend(FLUSH);
    }
    (
        [
            (header::CONTENT_TYPE, format!("application/x-{service}-advertisement")),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        body,
    )
        .into_response()
}

fn decode_body(headers: &HeaderMap, body: Bytes) -> std::io::Result<Vec<u8>> {
    if headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"))
    {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&body[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(body.to_vec())
    }
}

async fn upload_pack(
    State(s): State<AppState>,
    Path((project, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (repo, _actor) = match locate(&s, &headers, &project, &repo, Role::Read) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let input = match decode_body(&headers, body) {
        Ok(b) => b,
        Err(e) => return plain(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let mut cmd = Command::new("git");
    cmd.args(["upload-pack", "--stateless-rpc"])
        .arg(s.engine.git.path(repo.id))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(p) = protocol_env(&headers) {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
    });
    let stdout = child.stdout.take().unwrap();
    // Keep the child alive until the response stream ends.
    let stream = tokio_util::io::ReaderStream::new(stdout).chain(futures_util::stream::once(async move {
        let _ = child.wait().await;
        Ok::<Bytes, std::io::Error>(Bytes::new())
    }));
    (
        [
            (header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// The command list at the start of a push: updates and the client's capabilities.
#[derive(Debug, PartialEq)]
pub struct PushRequest {
    pub updates: Vec<RefUpdate>,
    pub caps: Vec<String>,
    /// Offset of the pack data after the commands.
    pub pack_offset: usize,
}

pub fn parse_push(data: &[u8]) -> Result<PushRequest, String> {
    let mut pos = 0;
    let mut updates = Vec::new();
    let mut caps = Vec::new();
    loop {
        let len_hex = data.get(pos..pos + 4).ok_or("truncated request")?;
        let len = usize::from_str_radix(std::str::from_utf8(len_hex).map_err(|_| "bad pkt-line")?, 16)
            .map_err(|_| "bad pkt-line length")?;
        if len == 0 {
            pos += 4;
            break;
        }
        if len < 4 {
            return Err("bad pkt-line length".into());
        }
        let line = data.get(pos + 4..pos + len).ok_or("truncated pkt-line")?;
        pos += len;
        let line = std::str::from_utf8(line).map_err(|_| "command is not UTF-8")?;
        let line = line.trim_end_matches('\n');
        let (cmd, cap_str) = match line.split_once('\0') {
            Some((c, k)) => (c, Some(k)),
            None => (line, None),
        };
        if let Some(k) = cap_str {
            caps = k.split(' ').filter(|c| !c.is_empty()).map(str::to_string).collect();
        }
        let mut parts = cmd.split(' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("bad command {cmd:?}"));
        };
        if old.len() != 40 || new.len() != 40 {
            return Err("only sha1 repositories are supported".into());
        }
        updates.push(RefUpdate { name: name.into(), old: old.into(), new: new.into() });
    }
    Ok(PushRequest { updates, caps, pack_offset: pos })
}

/// Build the report-status response, wrapped in side-band 1 if the client asked for it.
fn report(req_caps: &[String], unpack: &str, results: &[(String, Result<(), String>)], messages: &[String]) -> Vec<u8> {
    let mut inner = pkt(format!("unpack {unpack}\n").as_bytes());
    for (name, r) in results {
        match r {
            Ok(()) => inner.extend(pkt(format!("ok {name}\n").as_bytes())),
            Err(e) => inner.extend(pkt(format!("ng {name} {}\n", e.replace('\n', " ")).as_bytes())),
        }
    }
    inner.extend(FLUSH);
    let sideband = req_caps.iter().any(|c| c == "side-band-64k" || c == "side-band");
    let wants_report = req_caps.iter().any(|c| c == "report-status");
    if !sideband {
        return if wants_report { inner } else { Vec::new() };
    }
    let mut out = Vec::new();
    // `quiet` only silences progress; these are hints the pusher should see.
    for m in messages {
        let mut b = vec![2u8];
        b.extend(format!("{m}\n").as_bytes());
        out.extend(pkt(&b));
    }
    if wants_report {
        for chunk in inner.chunks(65515) {
            let mut b = vec![1u8];
            b.extend(chunk);
            out.extend(pkt(&b));
        }
    }
    out.extend(FLUSH);
    out
}

async fn receive_pack(
    State(s): State<AppState>,
    Path((project, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (repo, actor) = match locate(&s, &headers, &project, &repo_name, Role::Write) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let data = match decode_body(&headers, body) {
        Ok(b) => b,
        Err(e) => return plain(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let req = match parse_push(&data) {
        Ok(r) => r,
        Err(e) => return plain(StatusCode::BAD_REQUEST, &e),
    };
    let path = match s.engine.git.ensure(repo.id, &repo.default_branch).await {
        Ok(p) => p,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    };
    let respond = |body: Vec<u8>| {
        (
            [
                (header::CONTENT_TYPE, "application/x-git-receive-pack-result"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response()
    };

    // Store the objects first. Until a ref points at them they are unreachable and harmless.
    let pack = &data[req.pack_offset..];
    if req.updates.iter().any(|u| !u.is_delete()) && !pack.is_empty() {
        let out = crate::git::git_output(
            &["index-pack", "--stdin", "--fix-thin"],
            Some(&path),
            &[],
            Some(pack),
        )
        .await;
        let err = match out {
            Ok(o) if o.status.success() => None,
            Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Some(format!("{e:#}")),
        };
        if let Some(e) = err {
            let results: Vec<_> = req.updates.iter().map(|u| (u.name.clone(), Err("unpacker error".to_string()))).collect();
            return respond(report(&req.caps, &e.replace('\n', " "), &results, &[]));
        }
    }

    let atomic = req.caps.iter().any(|c| c == "atomic");
    let results = match s.engine.apply_ref_updates(&repo, Some(&actor), &req.updates, Source::Push, atomic).await {
        Ok(r) => r,
        Err(e) => req.updates.iter().map(|_| Err(format!("{e:#}"))).collect(),
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let mut messages = Vec::new();
    for (u, r) in req.updates.iter().zip(&results) {
        if r.is_ok() && !u.is_delete() {
            if let Some(branch) = u.branch().filter(|b| *b != repo.default_branch) {
                messages.push(format!(
                    "Open a review for {branch}: http://{host}/#/p/{}/reviews/new?repo={}&branch={branch}",
                    repo.project_id, repo.id
                ));
            }
        }
    }
    let named: Vec<_> = req.updates.iter().map(|u| u.name.clone()).zip(results).collect();
    respond(report(&req.caps, "ok", &named, &messages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_push_commands() {
        let mut data = pkt(format!("{} {} refs/heads/main\0report-status side-band-64k\n", "0".repeat(40), "a".repeat(40)).as_bytes());
        data.extend(pkt(format!("{} {} refs/heads/old\n", "b".repeat(40), "0".repeat(40)).as_bytes()));
        data.extend(FLUSH);
        data.extend(b"PACKxyz");
        let req = parse_push(&data).unwrap();
        assert_eq!(req.updates.len(), 2);
        assert_eq!(req.caps, vec!["report-status", "side-band-64k"]);
        assert!(req.updates[1].is_delete());
        assert_eq!(&data[req.pack_offset..], b"PACKxyz");
    }

    #[test]
    fn report_wraps_sideband() {
        let caps = vec!["report-status".to_string(), "side-band-64k".to_string()];
        let out = report(&caps, "ok", &[("refs/heads/a".into(), Ok(())), ("refs/heads/b".into(), Err("nope".into()))], &["hi".into()]);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\u{2}hi"));
        assert!(text.contains("ok refs/heads/a"));
        assert!(text.contains("ng refs/heads/b nope"));
        assert!(text.ends_with("0000"));
    }
}
