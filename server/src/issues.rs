use crate::engine::{Engine, RunCtx};
use crate::db::Status;
use crate::repos::{RefUpdate, Repo};
use anyhow::Result;
use std::sync::Arc;
pub const SCHEMA: &str = "";
pub async fn on_run_finished(_e: &Arc<Engine>, _c: &Arc<RunCtx>, _s: Status) -> Result<()> { Ok(()) }
pub async fn on_ref_update(_e: &Arc<Engine>, _r: &Repo, _a: Option<i64>, _u: &RefUpdate, _p: &std::path::Path) -> Result<()> { Ok(()) }
pub fn routes() -> axum::Router<crate::api::AppState> { axum::Router::new() }
pub fn valid_key(k: &str) -> bool { !k.is_empty() }
