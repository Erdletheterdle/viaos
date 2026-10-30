//! `via-agent` -- the viaos device agent.
//!
//! Runs on the host, outside the stack it manages. `mcp` serves the device's
//! tools over stdio, so `ssh device via-agent mcp` is a complete management
//! connection with no control plane involved.

mod host;
mod tools;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

use tools::Device;

#[derive(Parser)]
#[command(name = "via-agent", version, about = "viaos device agent")]
struct Cli {
    /// Where release state is kept.
    #[arg(long, env = "VIA_DATA_DIR", default_value = "/var/lib/via")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve this device's MCP tools on stdin/stdout.
    Mcp,
    /// Print device health as JSON.
    Health,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr: on `mcp`, stdout is the protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let device = Device::new(cli.data_dir);
    match cli.command {
        Command::Mcp => {
            device.serve(rmcp::transport::stdio()).await?.waiting().await?;
        }
        Command::Health => {
            println!("{}", serde_json::to_string_pretty(&device.health().await)?);
        }
    }
    Ok(())
}
