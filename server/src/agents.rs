//! Deploy agents. An agent runs on a target server and dials *out* to moloko over a
//! WebSocket (`moloko agent`), so targets need no inbound ports, SSH keys or VPN rules.
//! Pipeline jobs with `agent: <labels>` run their steps on every connected matching agent.

use crate::db::{Agent, Db};
use crate::executor::{Executor, StepResult};
use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToAgent {
    Exec {
        id: u64,
        script: String,
        env: Vec<(String, String)>,
    },
    Cancel {
        id: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromAgent {
    Hello {
        version: String,
        hostname: String,
        os: String,
    },
    Line {
        id: u64,
        line: String,
    },
    Exit {
        id: u64,
        code: i32,
    },
}

enum ExecEvent {
    Line(String),
    Exit(i32),
}

pub fn new_token() -> String {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .expect("read /dev/urandom");
    format!("mlk_{}", hex::encode(buf))
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[derive(Debug, Clone, Serialize)]
pub struct Presence {
    pub hostname: String,
    pub os: String,
    pub version: String,
    pub connected_at: i64,
}

pub struct Connection {
    pub agent: Agent,
    presence: Mutex<Presence>,
    tx: mpsc::UnboundedSender<ToAgent>,
    pending: Mutex<HashMap<u64, mpsc::UnboundedSender<ExecEvent>>>,
}

#[derive(Default)]
pub struct AgentHub {
    conns: Mutex<HashMap<i64, Arc<Connection>>>,
    seq: AtomicU64,
}

impl AgentHub {
    /// Connected agents that carry every one of `labels`.
    pub fn matching(&self, labels: &[String]) -> Vec<Arc<Connection>> {
        let mut v: Vec<_> = self
            .conns
            .lock()
            .unwrap()
            .values()
            .filter(|c| labels.iter().all(|l| c.agent.labels.contains(l)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.agent.name.cmp(&b.agent.name));
        v
    }

    pub fn presence(&self) -> HashMap<i64, Presence> {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .map(|(id, c)| (*id, c.presence.lock().unwrap().clone()))
            .collect()
    }

    /// Drive one agent connection until it closes.
    pub async fn serve(&self, socket: WebSocket, agent: Agent, db: Arc<Db>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<ToAgent>();
        let conn = Arc::new(Connection {
            agent: agent.clone(),
            presence: Mutex::new(Presence {
                hostname: String::new(),
                os: String::new(),
                version: String::new(),
                connected_at: crate::db::now_ms(),
            }),
            tx,
            pending: Mutex::default(),
        });
        // A reconnect replaces the previous connection for the same agent.
        self.conns.lock().unwrap().insert(agent.id, conn.clone());
        let _ = db.touch_agent(agent.id);
        tracing::info!(agent = agent.name, "agent connected");

        let (mut sink, mut stream) = socket.split();
        let writer = async {
            // Pings keep idle connections alive through proxies and tunnels.
            let mut ping = tokio::time::interval(Duration::from_secs(20));
            loop {
                let msg = tokio::select! {
                    m = rx.recv() => match m {
                        Some(m) => Message::Text(serde_json::to_string(&m).unwrap().into()),
                        None => break,
                    },
                    _ = ping.tick() => Message::Ping(Default::default()),
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        };
        let reader = async {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else {
                    if matches!(msg, Message::Close(_)) {
                        break;
                    }
                    continue;
                };
                match serde_json::from_str::<FromAgent>(&text) {
                    Ok(FromAgent::Hello {
                        version,
                        hostname,
                        os,
                    }) => {
                        let mut p = conn.presence.lock().unwrap();
                        (p.version, p.hostname, p.os) = (version, hostname, os);
                    }
                    Ok(FromAgent::Line { id, line }) => {
                        if let Some(tx) = conn.pending.lock().unwrap().get(&id) {
                            let _ = tx.send(ExecEvent::Line(line));
                        }
                    }
                    Ok(FromAgent::Exit { id, code }) => {
                        if let Some(tx) = conn.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(ExecEvent::Exit(code));
                        }
                    }
                    Err(e) => tracing::warn!(agent = agent.name, "bad message from agent: {e}"),
                }
            }
        };
        tokio::select! {
            _ = writer => {}
            _ = reader => {}
        }

        let mut conns = self.conns.lock().unwrap();
        if conns.get(&agent.id).is_some_and(|c| Arc::ptr_eq(c, &conn)) {
            conns.remove(&agent.id);
        }
        drop(conns);
        // Dropping pending senders fails any step still running on this agent.
        conn.pending.lock().unwrap().clear();
        let _ = db.touch_agent(agent.id);
        tracing::info!(agent = agent.name, "agent disconnected");
    }

    pub async fn exec(
        &self,
        conn: &Connection,
        script: &str,
        env: &[(String, String)],
        log: &(dyn Fn(&str) + Sync),
        cancel: &CancellationToken,
    ) -> Result<StepResult> {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        conn.pending.lock().unwrap().insert(id, tx);
        conn.tx
            .send(ToAgent::Exec {
                id,
                script: script.to_string(),
                env: env.to_vec(),
            })
            .map_err(|_| anyhow!("agent disconnected"))?;
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ExecEvent::Line(l)) => log(&l),
                    Some(ExecEvent::Exit(code)) => return Ok(StepResult::Exited(code)),
                    None => bail!("agent {} disconnected", conn.agent.name),
                },
                _ = cancel.cancelled() => {
                    conn.pending.lock().unwrap().remove(&id);
                    let _ = conn.tx.send(ToAgent::Cancel { id });
                    return Ok(StepResult::Cancelled);
                }
            }
        }
    }
}

/// `moloko agent`: connect to the server and run what it sends, reconnecting forever.
pub async fn run_agent(server: String, token: String, workdir: PathBuf) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match agent_session(&server, &token, &workdir).await {
            Ok(()) => tracing::warn!("connection closed by server"),
            Err(e) => tracing::warn!("connection failed: {e:#}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tracing::info!("reconnecting in {}s", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

pub async fn agent_session(server: &str, token: &str, workdir: &std::path::Path) -> Result<()> {
    use tokio_tungstenite::tungstenite::{Message as WsMsg, client::IntoClientRequest};

    let mut req = server.into_client_request().context("invalid server URL")?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("connect")?;
    tracing::info!("connected to {server}");
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<FromAgent>();
    let _ = tx.send(FromAgent::Hello {
        version: env!("CARGO_PKG_VERSION").into(),
        hostname: hostname(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    });
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink
                .send(WsMsg::Text(serde_json::to_string(&m).unwrap().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let running: Arc<Mutex<HashMap<u64, CancellationToken>>> = Arc::default();
    let session_cancel = CancellationToken::new();
    let result = async {
        while let Some(msg) = stream.next().await {
            let text = match msg? {
                WsMsg::Text(t) => t,
                WsMsg::Close(_) => break,
                _ => continue,
            };
            match serde_json::from_str::<ToAgent>(&text)? {
                ToAgent::Exec { id, script, env } => {
                    let cancel = session_cancel.child_token();
                    running.lock().unwrap().insert(id, cancel.clone());
                    let (tx, running, workdir) =
                        (tx.clone(), running.clone(), workdir.to_path_buf());
                    tokio::spawn(async move {
                        let line_tx = tx.clone();
                        let send_line = move |l: &str| {
                            let _ = line_tx.send(FromAgent::Line {
                                id,
                                line: l.to_string(),
                            });
                        };
                        let code = match Executor::Host
                            .run_step(&script, &workdir, &env, &send_line, &cancel)
                            .await
                        {
                            Ok(StepResult::Exited(c)) => c,
                            Ok(StepResult::Cancelled) => -1,
                            Err(e) => {
                                send_line(&format!("✗ {e:#}"));
                                -1
                            }
                        };
                        running.lock().unwrap().remove(&id);
                        let _ = tx.send(FromAgent::Exit { id, code });
                    });
                }
                ToAgent::Cancel { id } => {
                    if let Some(t) = running.lock().unwrap().remove(&id) {
                        t.cancel();
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    // Losing the server means nobody is watching: stop whatever is still running.
    session_cancel.cancel();
    writer.abort();
    result
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::git::tests::fixture_repo;
    use crate::logs::Subscription;

    async fn wait_run(engine: &Engine, run_id: i64) -> String {
        for _ in 0..200 {
            let run = engine.db.run(run_id).unwrap().unwrap();
            if !matches!(run.status.as_str(), "queued" | "running") {
                return run.status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run did not finish");
    }

    #[tokio::test]
    async fn agent_connects_out_and_runs_deploy_steps() {
        let dir = tempfile::tempdir().unwrap();
        let pipeline = r#"
jobs:
  build:
    steps: [echo built]
  deploy:
    needs: [build]
    agent: prod
    env: {APP: api}
    steps:
      - name: roll out
        run: echo "deploying $APP at $MOLOKO_SHA from $(pwd)"
  nowhere:
    agent: [staging]
    steps: [echo never]
"#;
        let repo = fixture_repo(dir.path(), &[(".moloko.yml", pipeline)]).await;
        let engine = Engine::new(&dir.path().join("data"), 2).unwrap();
        let pid = engine.db.create_project("demo", repo.to_str().unwrap()).unwrap();
        let token = new_token();
        engine.db.create_agent("web1", &["prod".into()], &hash_token(&token)).unwrap();

        let app = crate::api::router(crate::api::AppState { engine: engine.clone(), webhook_secret: None });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // A wrong token is rejected.
        let url = format!("ws://{addr}/api/agents/connect");
        assert!(agent_session(&url, "mlk_wrong", dir.path()).await.is_err());

        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let session = tokio::spawn({
            let (url, agent_dir) = (url.clone(), agent_dir.clone());
            async move { agent_session(&url, &token, &agent_dir).await }
        });
        // Wait until the agent is registered and its hello has arrived.
        for _ in 0..100 {
            if engine.agents.presence().values().any(|p| !p.os.is_empty()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(engine.agents.presence().values().any(|p| !p.os.is_empty()));

        let run = engine.trigger(pid, "main", None, "manual").unwrap();
        assert_eq!(wait_run(&engine, run).await, "failed"); // `nowhere` has no agent
        let jobs = engine.db.jobs(run).unwrap();
        let deploy = jobs.iter().find(|j| j.name == "deploy").unwrap();
        assert_eq!(deploy.status, "success");
        assert_eq!(jobs.iter().find(|j| j.name == "nowhere").unwrap().status, "failed");
        let Subscription::Done(log) = engine.logs.subscribe(deploy.id) else { panic!() };
        let sha = engine.db.run(run).unwrap().unwrap().sha.unwrap();
        assert!(log.contains(&format!("[web1] deploying api at {sha} from {}", agent_dir.canonicalize().unwrap().display())), "{log}");

        // When the agent goes away, its jobs fail instead of hanging.
        session.abort();
        for _ in 0..100 {
            if engine.agents.presence().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(engine.agents.presence().is_empty());
        assert!(engine.agents.matching(&["prod".into()]).is_empty());
    }
}
