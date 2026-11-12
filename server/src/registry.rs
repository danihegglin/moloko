//! Built-in OCI/Docker registry (Distribution Spec v2) served at `/v2/` on the same port
//! as the API. Pipelines push with a per-run token, and deploy agents pull with their
//! agent token, so built images never leave the build host until an agent asks for them.
//!
//! Layout under `<data>/registry`:
//!   blobs/sha256/<hex>               layer, config and manifest content (content-addressed)
//!   repos/<name>/manifests/<hex>     manifest link; the file holds its media type
//!   repos/<name>/tags/<tag>          "sha256:<hex>"
//!   uploads/<uuid>                   in-progress uploads

use anyhow::Result;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

const MAX_MANIFEST: usize = 4 << 20;
const DEFAULT_MANIFEST_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";

struct Upload {
    file: tokio::fs::File,
    hasher: Sha256,
    size: u64,
}

pub struct Registry {
    root: PathBuf,
    uploads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Upload>>>>,
    /// Hashes of read-write tokens handed to active runs.
    run_tokens: Mutex<HashSet<String>>,
}

#[derive(Debug, PartialEq)]
enum Route {
    Base,
    Catalog,
    Tags(String),
    Manifest(String, String),
    Blob(String, String),
    UploadStart(String),
    Upload(String, String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Access {
    Pull,
    PushPull,
}

fn valid_name(name: &str) -> bool {
    // OCI: path components of [a-z0-9]+ separated by one '.', '_', '__' or '-'s.
    !name.is_empty()
        && name.len() <= 255
        && name.split('/').all(|c| {
            !c.is_empty()
                && c.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())
                && c.ends_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())
                && c.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "._-".contains(ch))
                && !c.contains("..")
        })
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && !tag.starts_with(['.', '-'])
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Returns the hex part of a valid `sha256:` digest.
fn digest_hex(d: &str) -> Option<&str> {
    let hex = d.strip_prefix("sha256:")?;
    (hex.len() == 64
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
    .then_some(hex)
}

fn parse(tail: &str) -> Option<Route> {
    let tail = tail.trim_start_matches('/');
    if tail.is_empty() {
        return Some(Route::Base);
    }
    if tail == "_catalog" {
        return Some(Route::Catalog);
    }
    let route = if let Some(n) = tail.strip_suffix("/tags/list") {
        Route::Tags(n.into())
    } else if let Some(n) = tail
        .strip_suffix("/blobs/uploads")
        .or_else(|| tail.strip_suffix("/blobs/uploads/"))
    {
        Route::UploadStart(n.into())
    } else {
        let (head, last) = tail.rsplit_once('/')?;
        if let Some(n) = head.strip_suffix("/blobs/uploads") {
            if last.is_empty() || !last.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                return None;
            }
            Route::Upload(n.into(), last.into())
        } else if let Some(n) = head.strip_suffix("/manifests") {
            if digest_hex(last).is_none() && !valid_tag(last) {
                return None;
            }
            Route::Manifest(n.into(), last.into())
        } else {
            let n = head.strip_suffix("/blobs")?;
            digest_hex(last)?;
            Route::Blob(n.into(), last.into())
        }
    };
    let name = match &route {
        Route::Tags(n)
        | Route::UploadStart(n)
        | Route::Upload(n, _)
        | Route::Manifest(n, _)
        | Route::Blob(n, _) => n,
        _ => unreachable!(),
    };
    valid_name(name).then_some(route)
}

fn err(status: StatusCode, code: &str, msg: &str) -> Response {
    (
        status,
        axum::Json(json!({ "errors": [{ "code": code, "message": msg }] })),
    )
        .into_response()
}

fn unauthorized() -> Response {
    let mut r = err(
        StatusCode::UNAUTHORIZED,
        "UNAUTHORIZED",
        "authentication required",
    );
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"moloko\""),
    );
    r
}

fn hash(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn query_param(uri: &axum::http::Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.replace("%3A", ":").replace("%3a", ":"))
    })
}

fn with_headers(mut r: Response, pairs: &[(&'static str, String)]) -> Response {
    for (k, v) in pairs {
        if let Ok(v) = HeaderValue::from_str(v) {
            r.headers_mut().insert(*k, v);
        }
    }
    r
}

#[derive(Serialize)]
pub struct ImageTag {
    pub tag: String,
    pub digest: String,
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct ImageRepo {
    pub name: String,
    pub tags: Vec<ImageTag>,
}

impl Registry {
    pub fn new(root: &Path) -> Result<Self> {
        for d in ["blobs/sha256", "repos", "uploads"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        // Uploads don't survive a restart.
        for e in std::fs::read_dir(root.join("uploads"))?.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        Ok(Registry {
            root: root.to_path_buf(),
            uploads: Mutex::default(),
            run_tokens: Mutex::default(),
        })
    }

    pub fn grant_run_token(&self, token: &str) {
        self.run_tokens.lock().unwrap().insert(hash(token));
    }

    pub fn revoke_run_token(&self, token: &str) {
        self.run_tokens.lock().unwrap().remove(&hash(token));
    }

    /// Basic auth: a run token (any username) can push and pull; `<agent>:<agent token>` can pull.
    pub fn authorize(
        &self,
        headers: &HeaderMap,
        agent_lookup: impl Fn(&str) -> Option<String>,
    ) -> Option<Access> {
        let raw = headers
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Basic ")?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(raw.trim())
            .ok()?;
        let (user, pass) = std::str::from_utf8(&decoded).ok()?.split_once(':')?;
        if self.run_tokens.lock().unwrap().contains(&hash(pass)) {
            return Some(Access::PushPull);
        }
        (agent_lookup(pass).as_deref() == Some(user)).then_some(Access::Pull)
    }

    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root.join("blobs/sha256").join(hex)
    }

    fn repo(&self, name: &str) -> PathBuf {
        self.root.join("repos").join(name)
    }

    pub async fn handle(&self, req: Request, access: Option<Access>) -> Response {
        let path = req.uri().path().to_string();
        let tail = path.strip_prefix("/v2").unwrap_or("");
        let Some(route) = parse(tail) else {
            return err(
                StatusCode::NOT_FOUND,
                "NAME_INVALID",
                "invalid repository path",
            );
        };
        let method = req.method().clone();
        let Some(access) = access else {
            return unauthorized();
        };
        let read = method == Method::GET || method == Method::HEAD;
        if !read && access != Access::PushPull {
            return err(StatusCode::FORBIDDEN, "DENIED", "this token can only pull");
        }

        let resp = match (route, method.clone()) {
            (Route::Base, m) if read || m == Method::GET => StatusCode::OK.into_response(),
            (Route::Catalog, Method::GET) => {
                axum::Json(json!({ "repositories": self.repositories() })).into_response()
            }
            (Route::Tags(n), Method::GET) => {
                axum::Json(json!({ "name": n, "tags": self.tags(&n) })).into_response()
            }
            (Route::Blob(n, d), Method::GET | Method::HEAD) => self.get_blob(&n, &d, req).await,
            (Route::Blob(_, d), Method::DELETE) => {
                match std::fs::remove_file(self.blob_path(digest_hex(&d).unwrap())) {
                    Ok(()) => StatusCode::ACCEPTED.into_response(),
                    Err(_) => err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown"),
                }
            }
            (Route::UploadStart(n), Method::POST) => self.start_upload(&n, req).await,
            (Route::Upload(n, id), Method::PATCH) => self.patch_upload(&n, &id, req).await,
            (Route::Upload(n, id), Method::PUT) => self.finish_upload(&n, &id, req).await,
            (Route::Upload(n, id), Method::GET) => self.upload_status(&n, &id).await,
            (Route::Upload(_, id), Method::DELETE) => {
                self.uploads.lock().unwrap().remove(&id);
                let _ = tokio::fs::remove_file(self.root.join("uploads").join(&id)).await;
                StatusCode::NO_CONTENT.into_response()
            }
            (Route::Manifest(n, r), Method::GET | Method::HEAD) => {
                self.get_manifest(&n, &r, method == Method::HEAD).await
            }
            (Route::Manifest(n, r), Method::PUT) => self.put_manifest(&n, &r, req).await,
            (Route::Manifest(n, r), Method::DELETE) => self.delete_manifest(&n, &r).await,
            _ => err(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method not allowed",
            ),
        };
        with_headers(
            resp,
            &[("docker-distribution-api-version", "registry/2.0".into())],
        )
    }

    fn repositories(&self) -> Vec<String> {
        // Repositories are directories that contain a `tags` or `manifests` dir.
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !e.path().is_dir() || name == "tags" || name == "manifests" {
                    continue;
                }
                let full = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if e.path().join("manifests").is_dir() {
                    out.push(full.clone());
                }
                walk(&e.path(), &full, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.root.join("repos"), "", &mut out);
        out.sort();
        out
    }

    fn tags(&self, name: &str) -> Vec<String> {
        let mut tags: Vec<String> = std::fs::read_dir(self.repo(name).join("tags"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        tags.retain(|t| !t.ends_with(".tmp"));
        tags.sort();
        tags
    }

    /// Repositories with their tags, newest first, for the dashboard.
    pub fn images(&self) -> Vec<ImageRepo> {
        self.repositories()
            .into_iter()
            .map(|name| {
                let mut tags: Vec<ImageTag> = self
                    .tags(&name)
                    .into_iter()
                    .filter_map(|tag| {
                        let p = self.repo(&name).join("tags").join(&tag);
                        let digest = std::fs::read_to_string(&p).ok()?.trim().to_string();
                        let modified = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                        let updated_at = modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()?
                            .as_millis() as i64;
                        Some(ImageTag {
                            tag,
                            digest,
                            updated_at,
                        })
                    })
                    .collect();
                tags.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
                ImageRepo { name, tags }
            })
            .collect()
    }

    async fn get_blob(&self, _name: &str, digest: &str, req: Request) -> Response {
        let path = self.blob_path(digest_hex(digest).unwrap());
        if !path.is_file() {
            return err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown");
        }
        // ServeFile handles HEAD, Content-Length and Range requests.
        let resp = match tower_http::services::ServeFile::new(&path)
            .oneshot(req)
            .await
        {
            Ok(r) => r.map(Body::new),
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "read failed"),
        };
        with_headers(
            resp,
            &[
                ("docker-content-digest", digest.into()),
                ("content-type", "application/octet-stream".into()),
            ],
        )
    }

    async fn start_upload(&self, name: &str, req: Request) -> Response {
        let uri = req.uri().clone();
        // Cross-repository mount: the blob is already here, nothing to upload.
        if let Some(d) = query_param(&uri, "mount").filter(|d| digest_hex(d).is_some())
            && self.blob_path(digest_hex(&d).unwrap()).is_file() {
                return self.created(name, &d);
            }
        let id = uuid();
        let file = match tokio::fs::File::create(self.root.join("uploads").join(&id)).await {
            Ok(f) => f,
            Err(_) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "UNKNOWN",
                    "cannot create upload",
                );
            }
        };
        let upload = Arc::new(tokio::sync::Mutex::new(Upload {
            file,
            hasher: Sha256::new(),
            size: 0,
        }));
        self.uploads.lock().unwrap().insert(id.clone(), upload);
        // Monolithic upload: POST with the whole blob and ?digest=.
        if let Some(d) = query_param(&uri, "digest") {
            return self.finish(name, &id, Some(d), req.into_body()).await;
        }
        with_headers(
            StatusCode::ACCEPTED.into_response(),
            &[
                ("location", format!("/v2/{name}/blobs/uploads/{id}")),
                ("docker-upload-uuid", id),
                ("range", "0-0".into()),
            ],
        )
    }

    fn upload(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<Upload>>> {
        self.uploads.lock().unwrap().get(id).cloned()
    }

    async fn write_body(upload: &mut Upload, body: Body) -> std::io::Result<()> {
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            upload.hasher.update(&chunk);
            upload.size += chunk.len() as u64;
            upload.file.write_all(&chunk).await?;
        }
        Ok(())
    }

    fn progress(name: &str, id: &str, size: u64, status: StatusCode) -> Response {
        with_headers(
            status.into_response(),
            &[
                ("location", format!("/v2/{name}/blobs/uploads/{id}")),
                ("docker-upload-uuid", id.into()),
                ("range", format!("0-{}", size.saturating_sub(1))),
            ],
        )
    }

    async fn patch_upload(&self, name: &str, id: &str, req: Request) -> Response {
        let Some(upload) = self.upload(id) else {
            return err(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            );
        };
        let mut u = upload.lock().await;
        if Self::write_body(&mut u, req.into_body()).await.is_err() {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "upload write failed",
            );
        }
        Self::progress(name, id, u.size, StatusCode::ACCEPTED)
    }

    async fn upload_status(&self, name: &str, id: &str) -> Response {
        match self.upload(id) {
            Some(u) => Self::progress(name, id, u.lock().await.size, StatusCode::NO_CONTENT),
            None => err(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            ),
        }
    }

    async fn finish_upload(&self, name: &str, id: &str, req: Request) -> Response {
        let digest = query_param(req.uri(), "digest");
        self.finish(name, id, digest, req.into_body()).await
    }

    async fn finish(&self, name: &str, id: &str, expected: Option<String>, body: Body) -> Response {
        let Some(upload) = self.uploads.lock().unwrap().remove(id) else {
            return err(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            );
        };
        let tmp = self.root.join("uploads").join(id);
        let mut u = upload.lock().await;
        let written = Self::write_body(&mut u, body).await;
        let flushed = u.file.flush().await;
        if written.is_err() || flushed.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "upload write failed",
            );
        }
        let hex = hex::encode(u.hasher.clone().finalize());
        let digest = format!("sha256:{hex}");
        if expected.as_deref() != Some(digest.as_str()) {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "digest does not match content",
            );
        }
        if tokio::fs::rename(&tmp, self.blob_path(&hex)).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "blob store failed",
            );
        }
        self.created(name, &digest)
    }

    fn created(&self, name: &str, digest: &str) -> Response {
        with_headers(
            StatusCode::CREATED.into_response(),
            &[
                ("location", format!("/v2/{name}/blobs/{digest}")),
                ("docker-content-digest", digest.into()),
            ],
        )
    }

    fn resolve(&self, name: &str, reference: &str) -> Option<String> {
        if digest_hex(reference).is_some() {
            return Some(reference.to_string());
        }
        let d = std::fs::read_to_string(self.repo(name).join("tags").join(reference)).ok()?;
        Some(d.trim().to_string())
    }

    async fn get_manifest(&self, name: &str, reference: &str, head: bool) -> Response {
        let unknown = || {
            err(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            )
        };
        let Some(digest) = self.resolve(name, reference) else {
            return unknown();
        };
        let Some(hex) = digest_hex(&digest) else {
            return unknown();
        };
        let Ok(media_type) =
            tokio::fs::read_to_string(self.repo(name).join("manifests").join(hex)).await
        else {
            return unknown();
        };
        let Ok(data) = tokio::fs::read(self.blob_path(hex)).await else {
            return unknown();
        };
        let headers = [
            ("content-type", media_type),
            ("docker-content-digest", digest.clone()),
            ("content-length", data.len().to_string()),
        ];
        let body = if head {
            Body::empty()
        } else {
            Body::from(data)
        };
        with_headers(Response::new(body), &headers)
    }

    async fn put_manifest(&self, name: &str, reference: &str, req: Request) -> Response {
        let content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Ok(data) = axum::body::to_bytes(req.into_body(), MAX_MANIFEST).await else {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "MANIFEST_INVALID",
                "manifest too large",
            );
        };
        let parsed: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_INVALID",
                    "manifest is not JSON",
                );
            }
        };
        let media_type = parsed
            .get("mediaType")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(content_type)
            .unwrap_or_else(|| DEFAULT_MANIFEST_TYPE.into());
        let hex = hex::encode(Sha256::digest(&data));
        let digest = format!("sha256:{hex}");
        if digest_hex(reference).is_some() && reference != digest {
            return err(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "digest does not match manifest",
            );
        }
        // Image manifests must only reference blobs we have.
        let mut refs: Vec<&serde_json::Value> = parsed
            .get("layers")
            .and_then(|l| l.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        refs.extend(parsed.get("config"));
        for r in refs {
            let ok = r
                .get("digest")
                .and_then(|d| d.as_str())
                .and_then(digest_hex)
                .is_some_and(|h| self.blob_path(h).is_file());
            if !ok {
                return err(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_BLOB_UNKNOWN",
                    "manifest references an unknown blob",
                );
            }
        }

        let repo = self.repo(name);
        let write = async {
            tokio::fs::create_dir_all(repo.join("manifests")).await?;
            tokio::fs::create_dir_all(repo.join("tags")).await?;
            atomic_write(&self.blob_path(&hex), &data).await?;
            atomic_write(&repo.join("manifests").join(&hex), media_type.as_bytes()).await?;
            if digest_hex(reference).is_none() {
                atomic_write(&repo.join("tags").join(reference), digest.as_bytes()).await?;
            }
            std::io::Result::Ok(())
        };
        if write.await.is_err() {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "manifest store failed",
            );
        }
        with_headers(
            StatusCode::CREATED.into_response(),
            &[
                ("location", format!("/v2/{name}/manifests/{digest}")),
                ("docker-content-digest", digest),
            ],
        )
    }

    async fn delete_manifest(&self, name: &str, reference: &str) -> Response {
        let repo = self.repo(name);
        if digest_hex(reference).is_none() {
            return match tokio::fs::remove_file(repo.join("tags").join(reference)).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(_) => err(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "manifest unknown",
                ),
            };
        }
        let hex = digest_hex(reference).unwrap();
        if tokio::fs::remove_file(repo.join("manifests").join(hex))
            .await
            .is_err()
        {
            return err(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            );
        }
        // Drop tags pointing at it.
        for tag in self.tags(name) {
            let p = repo.join("tags").join(&tag);
            if std::fs::read_to_string(&p).is_ok_and(|d| d.trim() == reference) {
                let _ = std::fs::remove_file(p);
            }
        }
        StatusCode::ACCEPTED.into_response()
    }
}

async fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", uuid()));
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, path).await
}

fn uuid() -> String {
    let t = crate::agents::new_token();
    let h = &t[4..36];
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Lowercased, registry-safe repository component for a project name.
pub fn repo_component(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "project".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routes_and_rejects_traversal() {
        let d = format!("sha256:{}", "a".repeat(64));
        assert_eq!(parse(""), Some(Route::Base));
        assert_eq!(parse("/"), Some(Route::Base));
        assert_eq!(parse("_catalog"), Some(Route::Catalog));
        assert_eq!(
            parse("demo/api/tags/list"),
            Some(Route::Tags("demo/api".into()))
        );
        assert_eq!(
            parse("demo/api/manifests/main"),
            Some(Route::Manifest("demo/api".into(), "main".into()))
        );
        assert_eq!(
            parse(&format!("demo/blobs/{d}")),
            Some(Route::Blob("demo".into(), d.clone()))
        );
        assert_eq!(
            parse("demo/blobs/uploads/"),
            Some(Route::UploadStart("demo".into()))
        );
        assert_eq!(
            parse("demo/blobs/uploads/ab-12"),
            Some(Route::Upload("demo".into(), "ab-12".into()))
        );
        assert_eq!(parse("../etc/manifests/x"), None);
        assert_eq!(parse("demo/manifests/../../x"), None);
        assert_eq!(parse("Demo/manifests/x"), None);
        assert_eq!(parse("demo/blobs/sha256:short"), None);
        assert_eq!(parse("demo/blobs/uploads/../../x"), None);
    }

    #[test]
    fn repo_components() {
        assert_eq!(repo_component("My API_server"), "my-api-server");
        assert_eq!(repo_component("--"), "project");
    }
}

#[cfg(test)]
mod http_tests {
    use crate::engine::Engine;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    fn basic(user: &str, pass: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        )
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        auth: &str,
        body: Vec<u8>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", auth)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        (
            parts.status,
            parts.headers,
            axum::body::to_bytes(body, usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    fn digest(data: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(data)))
    }

    #[tokio::test]
    async fn push_pull_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(&dir.path().join("data"), 1, "localhost:0").unwrap();
        engine.registry.grant_run_token("runtok");
        let agent_token = crate::agents::new_token();
        engine
            .db
            .create_agent("web1", &[], &crate::agents::hash_token(&agent_token))
            .unwrap();
        let app = crate::api::router(crate::api::AppState {
            engine: engine.clone(),
            webhook_secret: None,
        });
        let rw = basic("moloko", "runtok");
        let ro = basic("web1", &agent_token);

        assert_eq!(
            send(&app, "GET", "/v2/", "", vec![]).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&app, "GET", "/v2/", &ro, vec![]).await.0,
            StatusCode::OK
        );

        // Chunked upload: POST, PATCH x2, PUT ?digest.
        let layer = b"layer-bytes-".repeat(1000);
        let (st, h, _) = send(&app, "POST", "/v2/demo/app/blobs/uploads/", &rw, vec![]).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let loc = h["location"].to_str().unwrap().to_string();
        assert_eq!(
            send(&app, "PATCH", &loc, &rw, layer[..5000].to_vec())
                .await
                .0,
            StatusCode::ACCEPTED
        );
        let (_, h, _) = send(&app, "PATCH", &loc, &rw, layer[5000..].to_vec()).await;
        assert_eq!(h["range"], format!("0-{}", layer.len() - 1));
        let (st, _, _) = send(
            &app,
            "PUT",
            &format!("{loc}?digest=sha256:{}", "0".repeat(64)),
            &rw,
            vec![],
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "wrong digest is rejected");

        let (_, h, _) = send(&app, "POST", "/v2/demo/app/blobs/uploads/", &rw, vec![]).await;
        let loc = h["location"].to_str().unwrap().to_string();
        send(&app, "PATCH", &loc, &rw, layer.clone()).await;
        let (st, h, _) = send(
            &app,
            "PUT",
            &format!("{loc}?digest={}", digest(&layer)),
            &rw,
            vec![],
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(h["docker-content-digest"], digest(&layer));

        // Monolithic upload of the config.
        let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
        let (st, _, _) = send(
            &app,
            "POST",
            &format!("/v2/demo/app/blobs/uploads/?digest={}", digest(&config)),
            &rw,
            config.clone(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        // A manifest pointing at a missing blob is rejected; a valid one is tagged.
        let missing = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"digest":"sha256:{}","size":1}},"layers":[]}}"#,
            "1".repeat(64)
        );
        assert_eq!(
            send(
                &app,
                "PUT",
                "/v2/demo/app/manifests/v1",
                &rw,
                missing.into_bytes()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
            digest(&config),
            config.len(),
            digest(&layer),
            layer.len()
        );
        let (st, h, _) = send(
            &app,
            "PUT",
            "/v2/demo/app/manifests/v1",
            &rw,
            manifest.clone().into_bytes(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(h["docker-content-digest"], digest(manifest.as_bytes()));

        // Agents pull by tag and digest, and can't push.
        let (st, h, body) = send(&app, "GET", "/v2/demo/app/manifests/v1", &ro, vec![]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            h["content-type"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(body, manifest.as_bytes());
        let (st, _, body) = send(
            &app,
            "GET",
            &format!("/v2/demo/app/blobs/{}", digest(&layer)),
            &ro,
            vec![],
        )
        .await;
        assert_eq!((st, body), (StatusCode::OK, layer.clone()));
        assert_eq!(
            send(&app, "GET", "/v2/demo/app/tags/list", &ro, vec![])
                .await
                .2,
            br#"{"name":"demo/app","tags":["v1"]}"#
        );
        assert_eq!(
            send(&app, "POST", "/v2/demo/app/blobs/uploads/", &ro, vec![])
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // Cross-repo mount skips the upload.
        let (st, _, _) = send(
            &app,
            "POST",
            &format!(
                "/v2/other/blobs/uploads/?mount={}&from=demo/app",
                digest(&layer)
            ),
            &rw,
            vec![],
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        let images = engine.registry.images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].tags[0].tag, "v1");

        // After the run ends its token stops working.
        engine.registry.revoke_run_token("runtok");
        assert_eq!(
            send(&app, "GET", "/v2/", &rw, vec![]).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
