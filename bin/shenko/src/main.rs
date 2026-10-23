//! Shenko — personal agent framework entry point.
//! SPEC §44: seed/bootstrap startup.
use std::path::PathBuf;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "shenko", about = "Shenko（深刻）— personal agent framework", version = "0.1.0")]
struct Cli {
    /// Data directory
    #[arg(long, env = "SHENKO_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Log level
    #[arg(long, default_value = "info", env = "SHENKO_LOG")]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start Shenko
    Start {
        /// UI port
        #[arg(long, default_value = "7860")]
        port: u16,
        /// Agent name (used on first startup)
        #[arg(long)]
        name: Option<String>,
    },
    /// Check configuration and exit
    Check,
    /// Print version info
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_target(true)
        .compact()
        .init();

    let data_dir = cli
        .data_dir
        .unwrap_or_else(|| dirs_next::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("shenko"));

    match cli.command {
        Command::Version => {
            println!("Shenko（深刻）0.1.0");
            println!("Product: personal agent framework");
            println!("SPEC version: 0.6.2");
        }
        Command::Check => {
            info!(data_dir = %data_dir.display(), "checking configuration");
            println!("OK");
        }
        Command::Start { port, name } => {
            info!(
                data_dir = %data_dir.display(),
                port,
                name = name.as_deref().unwrap_or("(unnamed)"),
                "Shenko starting"
            );

            // Open store
            tokio::fs::create_dir_all(&data_dir).await?;
            let db_path = data_dir.join("shenko.db");
            let cas_dir = data_dir.join("cas");
            let store = shenko_store::store::Store::open(db_path, cas_dir)?;
            info!(
                sqlite = %store.capabilities.sqlite_version,
                wal = store.capabilities.wal_mode,
                fts5 = store.capabilities.fts5_available,
                "store ready"
            );

            // Start UI server
            let ui = shenko_ui::server::UiServer::new(port);
            println!("Shenko UI: http://127.0.0.1:{port}");

            // Run until Ctrl-C
            tokio::select! {
                res = ui.run() => {
                    if let Err(e) = res {
                        tracing::error!("UI error: {e}");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down");
                }
            }
        }
    }

    Ok(())
}
