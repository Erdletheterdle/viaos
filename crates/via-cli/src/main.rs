//! `via` -- viaos tools for the operator's own machine.
//!
//! `via mcp` serves the tools that only make sense where the operator is:
//! writing a provisioning image to an SD card or USB disk plugged in here,
//! and building an app release from a compose file on this machine.
//! Uploading needs `VIA_SERVER` and `VIA_ADMIN_TOKEN`.
//! The fleet's tools live on the control plane; Claude uses both, e.g.
//! `create_provisioning_image` there, then `flash_image` here.

mod build;
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
    /// Build an app release from a compose file and upload it.
    Release {
        /// Compose file.
        #[arg(short, long, default_value = "compose.yaml")]
        file: std::path::PathBuf,
        /// Application name. Defaults to the compose file's directory name.
        #[arg(short, long)]
        app: Option<String>,
        /// Release number. Releases are immutable; use a new number each time.
        #[arg(short, long)]
        release: u64,
        /// Architectures to build for.
        #[arg(long, default_value = "x86_64")]
        arch: Vec<String>,
        /// Use images already present locally instead of `docker build`.
        #[arg(long)]
        no_build: bool,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReleaseArgs {
    /// Path to the compose file on this machine.
    compose_file: String,
    /// Application name. Defaults to the compose file's directory name.
    #[serde(default)]
    app: Option<String>,
    /// Release number. Releases are immutable: use the next unused number,
    /// which `list_releases` on the control plane shows.
    release: u64,
    /// Architectures to build for: `x86_64` and/or `aarch64`. Build for every
    /// architecture the fleet has. Defaults to x86_64.
    #[serde(default)]
    archs: Vec<String>,
    /// Use images already present locally instead of running `docker build`.
    #[serde(default)]
    no_build: bool,
}

/// Build a bundle per architecture and upload each to the control plane.
fn release(file: std::path::PathBuf, app: Option<String>, number: u64, archs: Vec<String>, no_build: bool) -> Result<Value> {
    let server = std::env::var("VIA_SERVER").map_err(|_| anyhow::anyhow!("set VIA_SERVER to the control plane URL"))?;
    let token = std::env::var("VIA_ADMIN_TOKEN").map_err(|_| anyhow::anyhow!("set VIA_ADMIN_TOKEN"))?;
    let archs = if archs.is_empty() { vec!["x86_64".to_string()] } else { archs };
    let out_dir = std::env::temp_dir().join(format!("via-release-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir)?;

    let mut results = Vec::new();
    let mut app_name = String::new();
    for arch in &archs {
        let platform = match arch.as_str() {
            "x86_64" => "linux/amd64",
            "aarch64" => "linux/arm64",
            other => anyhow::bail!("unknown arch {other:?}; use x86_64 or aarch64"),
        };
        let built = build::build(build::BuildArgs {
            file: Some(file.clone()),
            app: app.clone(),
            release: number,
            platform: Some(platform.into()),
            output: Some(out_dir.join(format!("{arch}.bundle"))),
            profile: Vec::new(),
            strict: false,
            no_build,
            compression: 3,
            sign_key: None,
        })?;
        let url = format!("{}/v1/releases/{}/{}/{arch}", server.trim_end_matches('/'), built.app, number);
        let resp = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()?
            .put(&url)
            .bearer_auth(&token)
            .body(std::fs::File::open(&built.path)?)
            .send()?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        anyhow::ensure!(status.is_success(), "upload of {arch} failed: {status} {text}");
        let _ = std::fs::remove_file(&built.path);
        app_name = built.app.clone();
        results.push(json!({
            "arch": arch,
            "sha256": built.sha256,
            "size_mb": built.size_bytes / 1_048_576,
            "warnings": built.warnings,
        }));
    }
    let _ = std::fs::remove_dir_all(&out_dir);
    Ok(json!({ "app": app_name, "release": number, "uploaded": results,
               "next": "Deploy it with `deploy` on the control plane (dry_run first)." }))
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
        description = "Build an app release from a compose file on this machine and upload it \
                       to the control plane: images are built or taken from the local Docker, \
                       packed into one self-contained bundle per architecture, and uploaded. \
                       Devices need no registry. Can take minutes. Then use `deploy` on the \
                       control plane.",
        annotations(title = "Release app", read_only_hint = false, destructive_hint = false)
    )]
    async fn release_app(&self, Parameters(a): Parameters<ReleaseArgs>) -> Result<CallToolResult, ErrorData> {
        let r = tokio::task::spawn_blocking(move || {
            release(a.compose_file.into(), a.app, a.release, a.archs, a.no_build)
        })
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        reported(r)
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
        Command::Release { file, app, release: number, arch, no_build } => {
            let r = tokio::task::spawn_blocking(move || release(file, app, number, arch, no_build)).await??;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
    }
    Ok(())
}
