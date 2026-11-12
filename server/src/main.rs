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
        /// Shared secret for GitHub webhooks. Webhooks are rejected when unset.
        #[arg(long, env = "MOLOKO_WEBHOOK_SECRET", hide_env_values = true)]
        webhook_secret: Option<String>,
    },
    /// Run a deploy agent on a target server. It connects out to the moloko server.
    Agent {
        /// WebSocket URL of the server, e.g. wss://moloko.example.ts.net/api/agents/connect
        #[arg(long, env = "MOLOKO_SERVER")]
        server: String,
        /// Agent token from the dashboard (Agents → Add agent).
        #[arg(long, env = "MOLOKO_AGENT_TOKEN", hide_env_values = true)]
        token: String,
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
        } => {
            let executors = executors.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });
            let registry_host = registry_host.unwrap_or_else(|| default_registry_host(&listen));
            let engine = engine::Engine::new(&data, executors, &registry_host)?;
            let app = api::router(api::AppState {
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
            server,
            token,
            workdir,
        } => {
            agents::run_agent(server, token, workdir).await?;
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
