use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::process::Command;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "duvm-ctl", about = "Control tool for duvm distributed memory")]
struct Cli {
    /// Unix socket path to connect to daemon
    #[arg(long, default_value = "/run/duvm/duvm.sock")]
    socket: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show daemon status
    Status,
    /// Show runtime statistics
    Stats,
    /// List active backends
    Backends,
    /// Ping the daemon
    Ping,

    /// Enable distributed memory on this node.
    /// Loads the kernel module, starts daemon + memserver, activates swap.
    Enable {
        /// Swap priority (higher = preferred over local swap). Default: 100.
        #[arg(long, default_value = "100")]
        priority: i32,

        /// Kernel module swap device size in MB. Default: 4096 (4GB).
        #[arg(long, default_value = "4096")]
        size_mb: u64,
    },

    /// Disable distributed memory on this node.
    /// Drains remote pages back, deactivates swap, stops services, unloads kmod.
    Disable,

    /// Drain all remote pages back to local memory.
    /// Runs swapoff on the duvm swap device (kernel migrates pages back).
    /// The daemon and memserver keep running — only swap is deactivated.
    Drain,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Status => daemon_command(&cli.socket, "status").await,
        Commands::Stats => daemon_command(&cli.socket, "stats").await,
        Commands::Backends => daemon_command(&cli.socket, "backends").await,
        Commands::Ping => daemon_command(&cli.socket, "ping").await,
        Commands::Enable { priority, size_mb } => cmd_enable(priority, size_mb).await,
        Commands::Disable => cmd_disable().await,
        Commands::Drain => cmd_drain().await,
    }
}

// ── Daemon socket commands ─────────────────────────────────────────

async fn daemon_command(socket_path: &str, cmd: &str) -> Result<()> {
    let response = send_command(socket_path, cmd).await?;

    // Pretty-print JSON responses
    if response.starts_with('{') || response.starts_with('[') {
        match serde_json::from_str::<serde_json::Value>(&response) {
            Ok(val) => println!("{}", serde_json::to_string_pretty(&val)?),
            Err(_) => println!("{}", response),
        }
    } else {
        println!("{}", response);
    }

    Ok(())
}

async fn send_command(socket_path: &str, cmd: &str) -> Result<String> {
    let stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "Failed to connect to daemon at {}. Is duvm-daemon running?",
            socket_path
        )
    })?;

    let (reader, mut writer) = stream.into_split();
    writer.write_all(cmd.as_bytes()).await?;
    writer.write_all(b"\n").await?;

    let mut reader = BufReader::new(reader);
    let mut response = String::new();
    reader.read_line(&mut response).await?;

    Ok(response.trim().to_string())
}

// ── Enable/Disable/Drain commands ──────────────────────────────────

fn run(cmd: &str, args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run: {} {}", cmd, args.join(" ")))?;
    Ok(output)
}

fn run_ok(cmd: &str, args: &[&str]) -> Result<()> {
    let output = run(cmd, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{} {} failed: {}", cmd, args.join(" "), stderr.trim());
    }
    Ok(())
}

fn is_module_loaded() -> bool {
    std::fs::read_to_string("/proc/modules")
        .map(|s| s.lines().any(|l| l.starts_with("duvm_kmod ")))
        .unwrap_or(false)
}

fn is_swap_active() -> bool {
    std::fs::read_to_string("/proc/swaps")
        .map(|s| s.lines().any(|l| l.contains("duvm_swap")))
        .unwrap_or(false)
}

fn is_service_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_root() -> Result<()> {
    if !nix::unistd::Uid::effective().is_root() {
        bail!("This command requires root privileges. Run with sudo.");
    }
    Ok(())
}

async fn cmd_enable(priority: i32, size_mb: u64) -> Result<()> {
    check_root()?;
    eprintln!("duvm: enabling distributed memory on this node...");

    // Step 1: Load kernel module if not already loaded
    if is_module_loaded() {
        eprintln!("  [1/4] Kernel module already loaded");
    } else {
        eprintln!("  [1/4] Loading kernel module (size_mb={})...", size_mb);
        run_ok("modprobe", &["duvm-kmod", &format!("size_mb={}", size_mb)])
            .or_else(|_| {
                // modprobe might fail if not installed via DKMS; try insmod
                let ko_path = find_kmod_path()?;
                run_ok("insmod", &[&ko_path, &format!("size_mb={}", size_mb)])
            })
            .context("Failed to load kernel module")?;
    }

    // Step 2: Prepare swap device
    if !std::path::Path::new("/dev/duvm_swap0").exists() {
        bail!("/dev/duvm_swap0 not found after loading kernel module");
    }

    if !is_swap_active() {
        eprintln!("  [2/4] Preparing swap device...");
        run_ok("mkswap", &["/dev/duvm_swap0"])?;
    } else {
        eprintln!("  [2/4] Swap device already active");
    }

    // Step 3: Start services
    eprintln!("  [3/4] Starting services...");
    if !is_service_active("duvm-memserver") {
        let _ = run_ok("systemctl", &["start", "duvm-memserver"]);
        if is_service_active("duvm-memserver") {
            eprintln!("        duvm-memserver: started");
        } else {
            eprintln!("        duvm-memserver: not available (start manually if needed)");
        }
    } else {
        eprintln!("        duvm-memserver: already running");
    }

    if !is_service_active("duvm-daemon") {
        let _ = run_ok("systemctl", &["start", "duvm-daemon"]);
        if is_service_active("duvm-daemon") {
            eprintln!("        duvm-daemon: started");
        } else {
            eprintln!("        duvm-daemon: not available (start manually if needed)");
        }
    } else {
        eprintln!("        duvm-daemon: already running");
    }

    // Step 4: Activate swap
    if !is_swap_active() {
        eprintln!("  [4/4] Activating swap (priority={})...", priority);
        run_ok("swapon", &["-p", &priority.to_string(), "/dev/duvm_swap0"])?;
    } else {
        eprintln!("  [4/4] Swap already active");
    }

    eprintln!();
    eprintln!("duvm: distributed memory ENABLED");
    eprintln!("  Swap device: /dev/duvm_swap0 (priority {})", priority);
    eprintln!("  Check status: duvm-ctl status");
    eprintln!("  Disable:      sudo duvm-ctl disable");
    Ok(())
}

async fn cmd_disable() -> Result<()> {
    check_root()?;
    eprintln!("duvm: disabling distributed memory on this node...");

    // Step 1: Drain (swapoff moves pages back to local RAM)
    if is_swap_active() {
        eprintln!("  [1/4] Draining remote pages (swapoff)...");
        eprintln!("         This may take a while if many pages are remote.");
        run_ok("swapoff", &["/dev/duvm_swap0"])
            .context("swapoff failed — pages may still be in use")?;
        eprintln!("         Done — all pages back in local RAM.");
    } else {
        eprintln!("  [1/4] Swap not active, nothing to drain");
    }

    // Step 2: Stop daemon
    if is_service_active("duvm-daemon") {
        eprintln!("  [2/4] Stopping daemon...");
        let _ = run_ok("systemctl", &["stop", "duvm-daemon"]);
    } else {
        eprintln!("  [2/4] Daemon not running");
    }

    // Step 3: Stop memserver
    if is_service_active("duvm-memserver") {
        eprintln!("  [3/4] Stopping memserver...");
        let _ = run_ok("systemctl", &["stop", "duvm-memserver"]);
    } else {
        eprintln!("  [3/4] Memserver not running");
    }

    // Step 4: Unload kernel module
    if is_module_loaded() {
        eprintln!("  [4/4] Unloading kernel module...");
        run_ok("rmmod", &["duvm_kmod"]).context("rmmod failed — device may still be in use")?;
    } else {
        eprintln!("  [4/4] Kernel module not loaded");
    }

    eprintln!();
    eprintln!("duvm: distributed memory DISABLED");
    Ok(())
}

async fn cmd_drain() -> Result<()> {
    check_root()?;

    if !is_swap_active() {
        eprintln!("duvm: swap not active, nothing to drain");
        return Ok(());
    }

    eprintln!("duvm: draining remote pages back to local memory...");
    eprintln!("  Running swapoff /dev/duvm_swap0");
    eprintln!("  The kernel will fault all remote pages back to local RAM.");
    eprintln!("  This may take a while depending on how many pages are remote.");
    eprintln!();

    run_ok("swapoff", &["/dev/duvm_swap0"])
        .context("swapoff failed — some pages may still be remote")?;

    eprintln!("duvm: drain complete — all pages back in local RAM.");
    eprintln!("  Daemon and memserver are still running.");
    eprintln!("  To re-enable swap: sudo swapon -p 100 /dev/duvm_swap0");
    Ok(())
}

fn find_kmod_path() -> Result<String> {
    // Try common locations
    let candidates = ["/lib/modules/duvm-kmod.ko", "/usr/local/lib/duvm-kmod.ko"];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return Ok(path.to_string());
        }
    }
    // Try current kernel's extra modules
    if let Ok(uname) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let kver = uname.trim();
        let path = format!("/lib/modules/{}/extra/duvm-kmod.ko", kver);
        if std::path::Path::new(&path).exists() {
            return Ok(path);
        }
    }
    bail!("duvm-kmod.ko not found. Build with: make -C duvm-kmod && sudo make -C duvm-kmod install")
}
