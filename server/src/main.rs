mod api;
mod cache;
mod config;
mod db;
mod engine;
mod executor;
mod git;
mod logs;

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
        /// Shared secret for GitHub webhooks. Webhooks are rejected when unset.
        #[arg(long, env = "MOLOKO_WEBHOOK_SECRET", hide_env_values = true)]
        webhook_secret: Option<String>,
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
        } => {
            let executors = executors.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });
            let engine = engine::Engine::new(&data, executors)?;
            let app = api::router(api::AppState {
                engine,
                webhook_secret: webhook_secret.filter(|s| !s.is_empty()),
            });
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!(
                "moloko listening on http://{listen} ({executors} executors, data in {})",
                data.display()
            );
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown())
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

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}
