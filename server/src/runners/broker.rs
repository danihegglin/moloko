//! `moloko broker`: the moloko cloud side of runners. It holds subscription keys and plan limits,
//! keeps a warm pool of runners per size, pairs a customer's lease with an idle runner, relays
//! their (sealed) frames, and records minutes. It never holds a key that opens a job.
//!
//! Provisioning VMs is outside this module: whatever boots runner VMs (a cloud autoscaler, or
//! `moloko runner --dev` for local testing) starts them pointed at `/v1/pool`.

use super::{Control, weight};
use anyhow::{Context, Result, bail};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    plan TEXT NOT NULL,
    minutes_cap INTEGER NOT NULL,
    concurrency INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS keys (
    hash TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id),
    created_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS leases (
    id TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL,
    size TEXT NOT NULL,
    assigned_at INTEGER NOT NULL,
    ended_at INTEGER
);
"#;

/// Plans from the concept: (name, included m-minutes per month, runners at a time).
pub const PLANS: &[(&str, i64, i64)] = &[
    ("trial", 500, 4),
    ("team", 3_000, 20),
    ("business", 40_000, 100),
];

type Idle = oneshot::Sender<(String, WebSocket)>;

pub struct Broker {
    db: Mutex<Connection>,
    pool_token_hash: String,
    idle: Mutex<HashMap<String, VecDeque<Idle>>>,
}

impl Broker {
    pub fn open(data: &Path, pool_token: &str) -> Result<Arc<Self>> {
        std::fs::create_dir_all(data)?;
        let db = Connection::open(data.join("broker.db"))?;
        db.execute_batch(SCHEMA)?;
        // Leases that were running when the broker stopped end now.
        db.execute("UPDATE leases SET ended_at = ?1 WHERE ended_at IS NULL", [crate::db::now_ms()])?;
        Ok(Arc::new(Broker {
            db: Mutex::new(db),
            pool_token_hash: crate::agents::hash_token(pool_token),
            idle: Mutex::default(),
        }))
    }

    /// Create an account on a plan (or update its plan) and issue a new subscription key.
    pub fn add_key(&self, account: &str, plan: &str) -> Result<String> {
        let Some((_, cap, conc)) = PLANS.iter().find(|p| p.0 == plan) else {
            bail!("unknown plan {plan:?}; plans: trial, team, business");
        };
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO accounts (name, plan, minutes_cap, concurrency) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET plan = ?2, minutes_cap = ?3, concurrency = ?4",
            params![account, plan, cap, conc],
        )?;
        let id: i64 = db.query_row("SELECT id FROM accounts WHERE name = ?1", [account], |r| r.get(0))?;
        let key = format!("mlkcloud_{}", hex::encode(super::random::<32>()));
        db.execute(
            "INSERT INTO keys (hash, account_id, created_at) VALUES (?1, ?2, ?3)",
            params![crate::agents::hash_token(&key), id, crate::db::now_ms()],
        )?;
        Ok(key)
    }

    fn account(&self, headers: &HeaderMap) -> Option<i64> {
        let token = bearer(headers)?;
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT account_id FROM keys WHERE hash = ?1 AND revoked = 0",
                [crate::agents::hash_token(token)],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// m-minutes used this calendar month (UTC), counting running leases up to now.
    pub fn usage(&self, account: i64) -> Result<(f64, i64)> {
        let now = crate::db::now_ms();
        let month_start = month_start_ms(now);
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT size, assigned_at, ended_at FROM leases WHERE account_id = ?1 AND assigned_at >= ?2",
        )?;
        let mut used = 0.0;
        let mut running = 0;
        for row in st.query_map(params![account, month_start], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?))
        })? {
            let (size, start, end) = row?;
            if end.is_none() {
                running += 1;
            }
            let minutes = ((end.unwrap_or(now) - start).max(0) + 59_999) / 60_000;
            used += minutes as f64 * weight(&size).unwrap_or(1.0);
        }
        Ok((used, running))
    }

    /// Check plan limits, take an idle runner and record the lease.
    fn assign(&self, account: i64, size: &str) -> Result<(String, Idle), (StatusCode, String)> {
        let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        let (cap, conc): (i64, i64) = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT minutes_cap, concurrency FROM accounts WHERE id = ?1", [account], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| internal(e.into()))?;
        let (used, running) = self.usage(account).map_err(internal)?;
        if used >= cap as f64 {
            return Err((StatusCode::PAYMENT_REQUIRED, format!("monthly minutes used up ({used:.0} of {cap})")));
        }
        if running >= conc {
            return Err((StatusCode::TOO_MANY_REQUESTS, format!("plan allows {conc} runners at a time")));
        }
        let runner = {
            let mut idle = self.idle.lock().unwrap();
            let q = idle.entry(size.to_string()).or_default();
            // Senders whose runner already disconnected are closed; skip them.
            loop {
                match q.pop_front() {
                    Some(tx) if tx.is_closed() => continue,
                    other => break other,
                }
            }
        };
        let Some(runner) = runner else {
            return Err((StatusCode::SERVICE_UNAVAILABLE, format!("no {size} runner available, try again shortly")));
        };
        let lease = format!("lse_{}", hex::encode(super::random::<8>()));
        self.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO leases (id, account_id, size, assigned_at) VALUES (?1, ?2, ?3, ?4)",
                params![lease, account, size, crate::db::now_ms()],
            )
            .map_err(|e| internal(e.into()))?;
        Ok((lease, runner))
    }

    fn end(&self, lease: &str) {
        let _ = self.db.lock().unwrap().execute(
            "UPDATE leases SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![lease, crate::db::now_ms()],
        );
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")
}

pub(crate) fn month_start_ms(now: i64) -> i64 {
    // Days since epoch -> civil date (Howard Hinnant's algorithm), then back to the 1st.
    let days = now.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    (days - (d - 1)) * 86_400_000
}

#[derive(Deserialize)]
struct SizeQuery {
    size: String,
}

pub fn router(broker: Arc<Broker>) -> Router {
    Router::new()
        .route("/v1/lease", get(lease))
        .route("/v1/pool", get(pool))
        .route("/v1/usage", get(usage))
        .with_state(broker)
}

/// Customer server side: `GET /v1/lease?size=m` as a WebSocket. The first frame is `leased` or
/// `refused`; after `leased`, frames are relayed to and from the runner unchanged.
async fn lease(
    State(b): State<Arc<Broker>>,
    headers: HeaderMap,
    Query(q): Query<SizeQuery>,
    up: WebSocketUpgrade,
) -> Response {
    let Some(account) = b.account(&headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid subscription key").into_response();
    };
    if weight(&q.size).is_none() {
        return (StatusCode::BAD_REQUEST, "size must be s, m, l or xl").into_response();
    }
    up.on_upgrade(move |mut ws| async move {
        match b.assign(account, &q.size) {
            Err((status, reason)) => {
                let _ = send(&mut ws, &Control::Refused { status: status.as_u16(), reason }).await;
            }
            Ok((lease, runner)) => {
                tracing::info!(lease, size = q.size, account, "lease assigned");
                // From here the runner's task relays and ends the lease. If either side is
                // already gone, end it now: dropping the socket closes the other end.
                let sent = send(&mut ws, &Control::Leased { lease: lease.clone() }).await.is_ok();
                if !sent || runner.send((lease.clone(), ws)).is_err() {
                    b.end(&lease);
                }
            }
        }
    })
}

/// Runner side: `GET /v1/pool?size=m` as a WebSocket, authenticated with the pool token. The
/// runner waits here until a lease arrives, then this task relays until either side closes.
async fn pool(
    State(b): State<Arc<Broker>>,
    headers: HeaderMap,
    Query(q): Query<SizeQuery>,
    up: WebSocketUpgrade,
) -> Response {
    let ok = bearer(&headers).is_some_and(|t| crate::agents::hash_token(t) == b.pool_token_hash);
    if !ok {
        return (StatusCode::UNAUTHORIZED, "invalid pool token").into_response();
    }
    if weight(&q.size).is_none() {
        return (StatusCode::BAD_REQUEST, "size must be s, m, l or xl").into_response();
    }
    up.on_upgrade(move |mut runner| async move {
        let (tx, rx) = oneshot::channel();
        b.idle.lock().unwrap().entry(q.size.clone()).or_default().push_back(tx);
        // Wait for a lease, noticing if the runner goes away first.
        let (lease, customer) = tokio::select! {
            r = rx => match r { Ok(v) => v, Err(_) => return },
            _ = async { while let Some(Ok(m)) = runner.next().await { if matches!(m, Message::Close(_)) { break } } } => return,
        };
        if send(&mut runner, &Control::Leased { lease: lease.clone() }).await.is_ok() {
            relay(runner, customer).await;
        }
        b.end(&lease);
        tracing::info!(lease, "lease ended");
    })
}

async fn usage(State(b): State<Arc<Broker>>, headers: HeaderMap) -> Response {
    let Some(account) = b.account(&headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid subscription key").into_response();
    };
    match b.usage(account) {
        Ok((used, running)) => Json(serde_json::json!({ "m_minutes": used, "running": running })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn send(ws: &mut WebSocket, msg: &Control) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(msg)?.into())).await.context("send")
}

/// Forward frames both ways until either side closes. The broker can't read them: after the
/// handshake they are sealed with keys only the two ends hold.
async fn relay(a: WebSocket, b: WebSocket) {
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    let ab = async {
        while let Some(Ok(m)) = a_rx.next().await {
            let close = matches!(m, Message::Close(_));
            if b_tx.send(m).await.is_err() || close {
                break;
            }
        }
    };
    let ba = async {
        while let Some(Ok(m)) = b_rx.next().await {
            let close = matches!(m, Message::Close(_));
            if a_tx.send(m).await.is_err() || close {
                break;
            }
        }
    };
    tokio::select! {
        _ = ab => {}
        _ = ba => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_start() {
        // 2026-11-26T14:34:16Z -> 2026-11-01T00:00:00Z
        assert_eq!(month_start_ms(1_795_703_656_000), 1_793_491_200_000);
    }
}
