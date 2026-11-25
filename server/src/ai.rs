use crate::engine::{Engine, RunCtx};
use crate::db::Status;
use anyhow::Result;
use std::sync::Arc;
pub const SCHEMA: &str = "";
#[derive(Default)]
pub struct AiHub;
pub async fn on_run_finished(_e: &Arc<Engine>, _c: &Arc<RunCtx>, _s: Status) -> Result<()> { Ok(()) }
pub fn routes() -> axum::Router<crate::api::AppState> { axum::Router::new() }
