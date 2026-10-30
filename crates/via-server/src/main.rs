//! `via-server` -- the viaos control plane.
//!
//! Devices connect out to it over one WebSocket each; people and Claude reach
//! it over MCP. There is no web UI: Claude is the UI.
//!
//! Routes:
//!
//! - `POST /v1/enroll`        a device spends its enrollment token
//! - `GET  /v1/connect`       a device's management connection (WebSocket)
//! - `GET  /v1/images/{id}`   download a provisioning image
//! - `/mcp`                   fleet MCP, Streamable HTTP, bearer token

mod db;
mod hub;
mod image;
mod mcp;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State as AxState, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use clap::Parser;
use serde::Deserialize;
use serde_json::json;

use db::Db;
use hub::Hub;

#[derive(Parser)]
#[command(name = "via-server", version, about = "viaos control plane")]
struct Cli {
    #[arg(long, env = "VIA_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Database and generated images live here.
    #[arg(long, env = "VIA_DATA_DIR", default_value = "/var/lib/via-server")]
    data_dir: PathBuf,
    /// Base viaos images, named `viaos-<arch>.raw`.
    #[arg(long, env = "VIA_IMAGES_DIR")]
    images_dir: Option<PathBuf>,
    /// The address devices and people use to reach this server, e.g.
    /// `https://via.example.com`. Written into provisioning images.
    #[arg(long, env = "VIA_PUBLIC_URL")]
    public_url: String,
    /// Bearer token for `/mcp`.
    #[arg(long, env = "VIA_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: String,
}

pub struct State {
    pub db: Db,
    pub hub: Hub,
    pub data_dir: PathBuf,
    pub images_dir: PathBuf,
    pub public_url: String,
    /// `public_url` with its scheme switched to WebSocket.
    pub device_url: String,
    admin_token_hash: String,
}

impl State {
    pub fn out_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,rmcp=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    anyhow::ensure!(cli.admin_token.len() >= 16, "VIA_ADMIN_TOKEN must be at least 16 characters");
    std::fs::create_dir_all(&cli.data_dir)?;

    let public_url = cli.public_url.trim_end_matches('/').to_string();
    let device_url = if let Some(rest) = public_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = public_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("VIA_PUBLIC_URL must start with http:// or https://");
    };
    let state = Arc::new(State {
        db: Db::open(&cli.data_dir.join("via.db"))?,
        hub: Hub::default(),
        images_dir: cli.images_dir.unwrap_or_else(|| cli.data_dir.join("base")),
        data_dir: cli.data_dir,
        public_url,
        device_url,
        admin_token_hash: db::hash(&cli.admin_token),
    });

    let mcp = Router::new()
        .nest_service("/mcp", mcp::service(state.clone()))
        .layer(middleware::from_fn_with_state(state.clone(), require_admin));
    let app = Router::new()
        .route("/v1/enroll", post(enroll))
        .route("/v1/connect", get(connect))
        .route("/v1/images/{id}", get(download))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state.clone())
        .merge(mcp);

    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    tracing::info!("listening on {} as {}", cli.listen, state.public_url);
    axum::serve(listener, app).await.context("server stopped")
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

async fn require_admin(AxState(s): AxState<Arc<State>>, req: Request, next: Next) -> Response {
    match bearer(req.headers()) {
        Some(t) if db::hash(t) == s.admin_token_hash => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "admin token required").into_response(),
    }
}

async fn enroll(AxState(s): AxState<Arc<State>>, Json(e): Json<db::Enrollment>) -> Response {
    match s.db.enroll(&e) {
        Ok((device, credential)) => {
            tracing::info!(device = %device.id, name = %device.name, fleet = %device.fleet, "enrolled");
            Json(json!({ "device": device, "credential": credential })).into_response()
        }
        Err(err) => (StatusCode::FORBIDDEN, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ConnectQuery {
    os_version: Option<String>,
    agent_version: Option<String>,
}

async fn connect(
    AxState(s): AxState<Arc<State>>,
    headers: HeaderMap,
    Query(q): Query<ConnectQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let device = match bearer(&headers).map(|c| s.db.authenticate(c)) {
        Some(Ok(Some(id))) => id,
        _ => return (StatusCode::UNAUTHORIZED, "unknown device credential").into_response(),
    };
    if let Err(e) = s.db.seen(&device, q.os_version.as_deref(), q.agent_version.as_deref()) {
        tracing::warn!("recording last-seen failed: {e}");
    }
    ws.on_upgrade(move |socket| async move {
        s.hub.attach(device.clone(), socket).await;
        let _ = s.db.seen(&device, None, None);
    })
}

/// The id is 96 random bits and the only way to name the file, so the link
/// itself is the credential. It is shown to whoever asked for the image.
async fn download(AxState(s): AxState<Arc<State>>, Path(id): Path<String>) -> Response {
    if id.len() != 24 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = s.out_dir().join(format!("{id}.raw.zst"));
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = std::fs::read_to_string(s.out_dir().join(format!("{id}.name")))
        .unwrap_or_else(|_| "viaos.raw.zst".into());
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    (
        [
            (header::CONTENT_TYPE, "application/zstd".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        body,
    )
        .into_response()
}
