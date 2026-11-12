mod agents;
mod api;
mod cache;
mod config;
mod db;
mod engine;
mod executor;
mod git;
mod logs;
mod registry;
mod tunnel;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "moloko", version, about = "Self-hosted build server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the build server and dashboard.
    Serve {
        /// Address to listen on. Defaults to localhost: there is no authentication yet.
        #[arg(long, env = "MOLOKO_LISTEN", default_value = "127.0.0.1:8080")]
        listen: String,
        /// Directory for the database, git mirrors, caches, logs and workspaces.
        #[arg(long, env = "MOLOKO_DATA", default_value = "moloko-data")]
        data: PathBuf,
        /// Maximum number of jobs running at once (default: CPU count).
        #[arg(long, env = "MOLOKO_EXECUTORS")]
        executors: Option<usize>,
        /// Host[:port] that jobs and agents use for the built-in registry (`docker push <host>/...`).
        /// Defaults to the listen address with `localhost` as host.
        #[arg(long, env = "MOLOKO_REGISTRY_HOST")]
        registry_host: Option<String>,
        /// UDP address for the built-in WireGuard endpoint agents connect to; `off` disables it.
        #[arg(long, env = "MOLOKO_WG_LISTEN", default_value = "0.0.0.0:51820")]
        wg_listen: String,
        /// host:port agents dial for WireGuard, written into join codes (e.g. your public IP or DNS name).
        #[arg(long, env = "MOLOKO_WG_ENDPOINT")]
        wg_endpoint: Option<String>,
        /// Shared secret for GitHub webhooks. Webhooks are rejected when unset.
        #[arg(long, env = "MOLOKO_WEBHOOK_SECRET", hide_env_values = true)]
        webhook_secret: Option<String>,
    },
    /// Run a deploy agent on a target server. It connects out to the moloko server.
    Agent {
        /// Join code from the dashboard (Agents → Add agent). Connects over built-in WireGuard.
        #[arg(long, env = "MOLOKO_JOIN", hide_env_values = true, conflicts_with_all = ["server", "token"])]
        join: Option<String>,
        /// Override the server's WireGuard endpoint (host:port) from the join code.
        #[arg(long, env = "MOLOKO_ENDPOINT", requires = "join")]
        endpoint: Option<String>,
        /// Local port for the registry proxy: `docker pull localhost:<port>/...`.
        #[arg(long, env = "MOLOKO_REGISTRY_PORT", default_value_t = 5000)]
        registry_port: u16,
        /// Without WireGuard: WebSocket URL of the server, e.g. wss://moloko.example.ts.net/api/agents/connect
        #[arg(long, env = "MOLOKO_SERVER", requires = "token")]
        server: Option<String>,
        /// Without WireGuard: agent token.
        #[arg(
            long,
            env = "MOLOKO_AGENT_TOKEN",
            hide_env_values = true,
            requires = "server"
        )]
        token: Option<String>,
        /// Directory deploy steps run in.
        #[arg(long, env = "MOLOKO_WORKDIR", default_value = ".")]
        workdir: PathBuf,
    },
    /// Check a .moloko.yml and print the job graph.
    Validate {
        #[arg(default_value = config::PIPELINE_FILE)]
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Serve {
            listen,
            data,
            executors,
            webhook_secret,
            registry_host,
            wg_listen,
            wg_endpoint,
        } => {
            let executors = executors.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });
            let registry_host = registry_host.unwrap_or_else(|| default_registry_host(&listen));
            let engine = engine::Engine::new(&data, executors, &registry_host)?;
            let wg = if wg_listen == "off" {
                None
            } else {
                Some(
                    start_wireguard(
                        &engine,
                        &data,
                        &listen,
                        &wg_listen,
                        wg_endpoint,
                        &registry_host,
                    )
                    .await?,
                )
            };
            let app = api::router(api::AppState {
                wg,
                engine,
                webhook_secret: webhook_secret.filter(|s| !s.is_empty()),
            });
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!(
                "moloko listening on http://{listen} ({executors} executors, registry {registry_host}, data in {})",
                data.display()
            );
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Command::Agent {
            join,
            endpoint,
            registry_port,
            server,
            token,
            workdir,
        } => match (join, server, token) {
            (Some(join), _, _) => {
                let join = tunnel::JoinCode::decode(&join)?;
                agents::run_agent_wireguard(join, endpoint, registry_port, workdir).await?;
            }
            (None, Some(server), Some(token)) => {
                agents::run_agent(server, token, workdir, None).await?;
            }
            _ => anyhow::bail!("pass --join <code>, or --server and --token"),
        },
        Command::Validate { file } => {
            let pipeline = config::Pipeline::parse(&std::fs::read_to_string(&file)?)?;
            for job in pipeline.expand() {
                if job.needs.is_empty() {
                    println!("{}", job.name);
                } else {
                    println!("{}  <- {}", job.name, job.needs.join(", "));
                }
            }
        }
    }
    Ok(())
}

/// Start the server side of the built-in WireGuard: one UDP socket, every agent with a key
/// as a peer, and tunnel TCP port 80 forwarded to the local HTTP listener.
async fn start_wireguard(
    engine: &engine::Engine,
    data: &std::path::Path,
    listen: &str,
    wg_listen: &str,
    wg_endpoint: Option<String>,
    registry_host: &str,
) -> Result<api::WgServer> {
    let key_file = data.join("wireguard.key");
    let private = match std::fs::read_to_string(&key_file) {
        Ok(k) => tunnel::key_from_b64(&k)?,
        Err(_) => {
            let k = tunnel::new_private_key();
            std::fs::write(&key_file, tunnel::b64(&k))?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600))?;
            k
        }
    };
    let mut peers = Vec::new();
    for (id, key) in engine.db.agent_wg_keys()? {
        if let Ok(k) = tunnel::key_from_b64(&key) {
            peers.push(api::WgServer::peer(id, k));
        }
    }
    let mut target: std::net::SocketAddr = listen.parse()?;
    if target.ip().is_unspecified() {
        target.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let udp = tunnel::bind_udp(wg_listen).await?;
    let port = udp.local_addr()?.port();
    let netstack = tunnel::spawn(
        udp,
        tunnel::NetstackConfig {
            private,
            ip: tunnel::SERVER_IP,
            peers,
            accept: Some((tunnel::TUNNEL_PORT, target)),
        },
    );
    let endpoint = wg_endpoint.unwrap_or_else(|| {
        let host = registry_host
            .rsplit_once(':')
            .map(|h| h.0)
            .unwrap_or(registry_host);
        format!("{host}:{port}")
    });
    tracing::info!("WireGuard on udp {wg_listen}, agents dial {endpoint}");
    Ok(api::WgServer {
        netstack,
        public: tunnel::public_key(&private),
        endpoint,
    })
}

/// `127.0.0.1:8080` / `0.0.0.0:8080` -> `localhost:8080`. Docker allows plain HTTP for localhost.
fn default_registry_host(listen: &str) -> String {
    match listen.rsplit_once(':') {
        Some((_, port)) => format!("localhost:{port}"),
        None => "localhost".into(),
    }
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}
