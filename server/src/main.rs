mod agents;
mod ai;
mod api;
mod auth;
mod cache;
mod code;
mod config;
mod crypto;
mod db;
mod engine;
mod executor;
mod git;
mod githttp;
mod issues;
mod logs;
mod mirror;
mod registry;
mod repos;
mod reviews;
mod runners;
mod tunnel;
mod users_api;

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
        /// Address to listen on. Create the first admin in the dashboard before exposing it.
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
        /// How often mirrored repos poll upstream, in seconds (webhooks make it immediate).
        #[arg(long, env = "MOLOKO_MIRROR_INTERVAL", default_value_t = 300)]
        mirror_interval: u64,
        #[command(flatten)]
        cloud: CloudArgs,
    },
    /// Run the moloko cloud broker: subscription keys, the runner warm pool, the relay, metering.
    Broker {
        #[command(subcommand)]
        command: BrokerCommand,
    },
    /// Run a cloud runner inside a confidential VM: take one job from the broker, run it, exit.
    Runner {
        /// Broker base URL, e.g. https://runners.example.com
        #[arg(long, env = "MOLOKO_BROKER")]
        broker: String,
        /// Token that lets runners join the broker's warm pool.
        #[arg(long, env = "MOLOKO_POOL_TOKEN", hide_env_values = true)]
        pool_token: String,
        /// Size class this VM was booted as: s, m, l or xl.
        #[arg(long, env = "MOLOKO_RUNNER_SIZE", default_value = "m")]
        size: String,
        /// No hardware attestation, for local development. Servers refuse it unless they run
        /// with --cloud-insecure-dev.
        #[arg(long)]
        dev: bool,
        /// AMD product for fetching the chip's VCEK certificate: Milan or Genoa.
        #[arg(long, env = "MOLOKO_AMD_PRODUCT", default_value = "Milan")]
        amd_product: String,
        /// VCEK certificate (DER) to use instead of fetching it from AMD.
        #[arg(long, env = "MOLOKO_VCEK")]
        vcek: Option<PathBuf>,
        /// Scratch directory for the job. Use the VM's encrypted scratch disk.
        #[arg(long, env = "MOLOKO_WORKDIR", default_value = "/var/lib/moloko-runner")]
        workdir: PathBuf,
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

/// How `moloko serve` reaches moloko runners.
#[derive(clap::Args)]
struct CloudArgs {
    /// moloko runners subscription key (`mlkcloud_…`). Enables `runner: cloud` jobs.
    #[arg(long, env = "MOLOKO_CLOUD_KEY", hide_env_values = true)]
    cloud_key: Option<String>,
    /// Broker URL for moloko runners.
    #[arg(long, env = "MOLOKO_CLOUD_BROKER")]
    cloud_broker: Option<String>,
    /// Launch measurements (hex) of runner images this server accepts, comma-separated.
    #[arg(long, env = "MOLOKO_CLOUD_MEASUREMENTS", value_delimiter = ',')]
    cloud_measurements: Vec<String>,
    /// Accept runners without hardware attestation. For local development only: such a runner's
    /// operator can read everything the job sends.
    #[arg(long, env = "MOLOKO_CLOUD_INSECURE_DEV")]
    cloud_insecure_dev: bool,
}

impl CloudArgs {
    fn config(self) -> Result<Option<runners::CloudConfig>> {
        match (self.cloud_key, self.cloud_broker) {
            (None, _) => Ok(None),
            (Some(_), None) => anyhow::bail!("--cloud-key needs --cloud-broker"),
            (Some(key), Some(broker)) => Ok(Some(runners::CloudConfig {
                broker,
                key,
                policy: runners::attest::Policy {
                    measurements: self.cloud_measurements,
                    insecure_dev: self.cloud_insecure_dev,
                },
            })),
        }
    }
}

#[derive(Subcommand)]
enum BrokerCommand {
    /// Serve the broker API.
    Serve {
        #[arg(long, env = "MOLOKO_BROKER_LISTEN", default_value = "127.0.0.1:8090")]
        listen: String,
        #[arg(long, env = "MOLOKO_BROKER_DATA", default_value = "moloko-broker")]
        data: PathBuf,
        /// Token runner VMs present to join the warm pool.
        #[arg(long, env = "MOLOKO_POOL_TOKEN", hide_env_values = true)]
        pool_token: String,
    },
    /// Create an account (or change its plan) and print a new subscription key.
    AddKey {
        #[arg(long, env = "MOLOKO_BROKER_DATA", default_value = "moloko-broker")]
        data: PathBuf,
        #[arg(long)]
        account: String,
        /// trial, team or business
        #[arg(long, default_value = "trial")]
        plan: String,
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
            mirror_interval,
            cloud,
        } => {
            let executors = executors.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });
            let registry_host = registry_host.unwrap_or_else(|| default_registry_host(&listen));
            let engine = engine::Engine::new(&data, executors, &registry_host)?;
            if let Some(cfg) = cloud.config()? {
                if cfg.policy.insecure_dev {
                    tracing::warn!("--cloud-insecure-dev: cloud runners are NOT verified; use only for development");
                }
                tracing::info!("moloko runners enabled via {}", cfg.broker);
                let _ = engine.cloud.set(cfg);
            }
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
            mirror::spawn_loop(engine.clone(), std::time::Duration::from_secs(mirror_interval.max(10)));
            crate::reviews::spawn_queue(engine.clone());
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
        Command::Broker { command } => match command {
            BrokerCommand::Serve { listen, data, pool_token } => {
                let broker = runners::broker::Broker::open(&data, &pool_token)?;
                let listener = tokio::net::TcpListener::bind(&listen).await?;
                tracing::info!("moloko broker listening on http://{listen}");
                axum::serve(listener, runners::broker::router(broker))
                    .with_graceful_shutdown(shutdown())
                    .await?;
            }
            BrokerCommand::AddKey { data, account, plan } => {
                // The pool token isn't needed to issue keys.
                let broker = runners::broker::Broker::open(&data, "")?;
                println!("{}", broker.add_key(&account, &plan)?);
            }
        },
        Command::Runner { broker, pool_token, size, dev, amd_product, vcek, workdir } => {
            anyhow::ensure!(runners::weight(&size).is_some(), "size must be s, m, l or xl");
            runners::runner::run(runners::runner::Options {
                broker,
                pool_token,
                size,
                dev,
                amd_product,
                vcek,
                workdir,
            })
            .await?;
        }
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
#[cfg(test)]
mod e2e_tests;
