//! `via` -- viaos tools for the operator's own machine.
//!
//! `via mcp` serves the tools that only make sense where the operator is:
//! writing a provisioning image to an SD card or USB disk plugged in here.
//! The fleet's tools live on the control plane; Claude uses both, e.g.
//! `create_provisioning_image` there, then `flash_image` here.

mod flash;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "via", version, about = "viaos operator tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve local tools (disk flashing) over MCP on stdin/stdout.
    Mcp,
    /// List disks and whether `flash_image` would write to them.
    Disks,
}

#[derive(Clone)]
struct Local {
    flasher: flash::Flasher,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FlashArgs {
    /// Image to write: the `download` URL from `create_provisioning_image`,
    /// or a local file. zstd-compressed images are decompressed on the fly.
    source: String,
    /// Disk path from `list_disks`, e.g. /dev/sdb. Must be writable there.
    disk: String,
    /// The disk's `size_bytes` from `list_disks`. Writing is refused if it
    /// no longer matches, which catches a card swapped in the meantime.
    size_bytes: u64,
}

fn rendered(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut r = CallToolResult::structured(value);
    r.content = vec![ContentBlock::text(text)];
    r
}

fn reported(result: Result<Value>) -> Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(v) => rendered(v),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
    })
}

#[tool_router(router = tool_router)]
impl Local {
    #[tool(
        description = "Disks attached to this machine: path, size, model, transport, and \
                       whether flash_image will write to it. Only unmounted removable or USB \
                       disks are writable. Show the user the model and size before flashing. \
                       Reads only.",
        annotations(title = "List disks", read_only_hint = true, open_world_hint = false)
    )]
    async fn list_disks(&self) -> Result<CallToolResult, ErrorData> {
        reported(flash::list().map(|d| json!({ "disks": d })))
    }

    #[tool(
        description = "ERASE a removable disk and write a viaos image to it. Everything on the \
                       disk is lost. Confirm the disk's model and size with the user first. Runs \
                       in the background; `flash_status` reports progress. When it is done, put \
                       the card or disk in the device and power it on.",
        annotations(title = "Flash image", read_only_hint = false, destructive_hint = true)
    )]
    async fn flash_image(&self, Parameters(a): Parameters<FlashArgs>) -> Result<CallToolResult, ErrorData> {
        reported(
            self.flasher
                .start(a.source, a.disk.clone(), a.size_bytes)
                .map(|()| json!({ "disk": a.disk, "state": "writing" })),
        )
    }

    #[tool(
        description = "Progress of the current or last flash_image. Reads only.",
        annotations(title = "Flash status", read_only_hint = true, open_world_hint = false)
    )]
    async fn flash_status(&self) -> Result<CallToolResult, ErrorData> {
        reported(Ok(json!({ "job": self.flasher.status() })))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Local {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("via-local", env!("CARGO_PKG_VERSION")))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Mcp => {
            let local = Local { flasher: Default::default(), tool_router: Local::tool_router() };
            local.serve(rmcp::transport::stdio()).await?.waiting().await?;
        }
        Command::Disks => println!("{}", serde_json::to_string_pretty(&flash::list()?)?),
    }
    Ok(())
}
