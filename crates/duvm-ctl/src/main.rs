use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "duvm-ctl", about = "Control tool for duvm daemon")]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let cmd = match cli.command {
        Commands::Status => "status",
        Commands::Stats => "stats",
        Commands::Backends => "backends",
        Commands::Ping => "ping",
    };

    let response = send_command(&cli.socket, cmd).await?;

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
