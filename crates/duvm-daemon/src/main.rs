use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

pub mod config;
pub mod engine;
pub mod kmod_ring;
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

    /// Path to kernel module control device
    #[arg(long, default_value = "/dev/duvm_ctl")]
    kmod_ctl: String,
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

    let eng = engine::Engine::new(config)?;

    // Try to connect to the kernel module ring buffer
    if !args.uffd_mode {
        match kmod_ring::KmodRingConsumer::open(&args.kmod_ctl) {
            Ok(consumer) => {
                tracing::info!(
                    path = args.kmod_ctl,
                    "Connected to kernel module — processing swap I/O"
                );

                // Run ring consumer in a background thread (it's a tight poll loop)
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stop_clone = stop.clone();

                let consumer_thread = std::thread::spawn(move || {
                    consumer.run_loop(&eng, &stop_clone);
                });

                // Wait for Ctrl+C
                tokio::signal::ctrl_c().await?;
                tracing::info!("Shutting down");
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                consumer_thread.join().ok();
                return Ok(());
            }
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "Kernel module not available — running in daemon-only mode"
                );
            }
        }
    }

    // Fallback: run as control socket server only (no kmod)
    let mut eng = engine::Engine::new(engine::Engine::default_config())?;
    eng.run().await?;

    Ok(())
}
