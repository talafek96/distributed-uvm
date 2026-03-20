use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

pub mod config;
pub mod engine;
pub mod policy;
pub mod uffd;

#[derive(Parser)]
#[command(name = "duvm-daemon", about = "Distributed UVM daemon")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/duvm/duvm.toml")]
    config: String,

    /// Run in userfaultfd-only mode (no kernel module)
    #[arg(long)]
    uffd_mode: bool,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Unix socket path for control
    #[arg(long, default_value = "/run/duvm/duvm.sock")]
    socket: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .json()
        .init();

    tracing::info!("duvm-daemon starting");

    let mut config = config::DaemonConfig::load_or_default(&args.config);

    // Apply CLI overrides (CLI args take precedence over config file)
    let socket_override = if args.socket != "/run/duvm/duvm.sock" {
        Some(args.socket.as_str())
    } else {
        None
    };
    config.apply_cli_overrides(socket_override, None);

    tracing::info!(?config, "Configuration loaded");

    let mut eng = engine::Engine::new(config)?;
    eng.run().await?;

    Ok(())
}
