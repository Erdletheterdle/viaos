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
//! - `GET  /v1/os/{arch}/{version}/{file}`  OS update files, for devices
//! - `PUT  /v1/os/{arch}/{version}/{file}`  upload OS update files (admin)
//! - `PUT  /v1/base/{arch}`   upload the base image provisioning starts from (admin)
//! - `PUT  /v1/releases/{app}/{release}/{arch}`  upload a bundle (admin)
//! - `GET  /v1/releases/{app}/{release}/{arch}`  fetch a bundle (devices)
//! - `GET  /v1/devices/{device}/tunnel/{port}`  a TCP stream to a port on a
//!   device, over its connection (WebSocket, admin)
//! - `/mcp`                   fleet MCP, Streamable HTTP, bearer token
//!
//! People authenticate with API tokens made by `via-server token create`.
//! Viewers get the read-only tools; admins get everything.

mod db;
mod hub;
mod image;
mod mcp;
mod os;
mod releases;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, Query, Request, State as AxState, WebSocketUpgrade};
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
    /// Database and generated images live here.
    #[arg(long, env = "VIA_DATA_DIR", default_value = "/var/lib/via-server", global = true)]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: Serve,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Run the control plane (the default).
    Serve,
    /// Manage the API tokens people and Claude use.
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(clap::Subcommand)]
enum TokenCommand {
    /// Create a token and print it. It is shown only this once.
    Create {
        /// Who it is for, e.g. `ana` or `ci`.
        name: String,
        #[arg(long, value_enum, default_value = "admin")]
        role: db::Role,
    },
    /// List tokens: name, role, and when each was last used.
    List,
    /// Revoke a token at once.
    Revoke { name: String },
}

#[derive(clap::Args)]
struct Serve {
    #[arg(long, env = "VIA_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Base viaos images, named `viaos-<arch>.raw`.
    #[arg(long, env = "VIA_IMAGES_DIR")]
    images_dir: Option<PathBuf>,
    /// OS updates, as `<arch>/<version>/` holding `nix build .#updates.<arch>`.
    #[arg(long, env = "VIA_OS_DIR")]
    os_dir: Option<PathBuf>,
    /// The address devices and people use to reach this server, e.g.
    /// `https://via.example.com`. Written into provisioning images.
    #[arg(long, env = "VIA_PUBLIC_URL")]
    public_url: Option<String>,
    /// An admin token that needs no database entry, for bootstrapping and
    /// for deployments that keep the secret elsewhere. Audited as `admin`.
    #[arg(long, env = "VIA_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Public keys (hex, from `via keygen`) that devices require OS updates
    /// and app releases to be signed by. Written into every provisioning
    /// image. Comma-separated.
    #[arg(long, env = "VIA_TRUSTED_KEYS", value_delimiter = ',')]
    trusted_keys: Vec<String>,
}

pub struct State {
    pub db: Db,
    pub hub: Hub,
    pub data_dir: PathBuf,
    pub images_dir: PathBuf,
    pub os_dir: PathBuf,
    pub public_url: String,
    /// `public_url` with its scheme switched to WebSocket.
    pub device_url: String,
    pub trusted_keys: Vec<String>,
    admin_token_hash: Option<String>,
}

impl State {
    pub fn out_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    pub fn releases_dir(&self) -> PathBuf {
        self.data_dir.join("releases")
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
    std::fs::create_dir_all(&cli.data_dir)?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(cli.serve, cli.data_dir).await,
        Command::Token(t) => {
            let db = Db::open(&cli.data_dir.join("via.db"))?;
            match t {
                TokenCommand::Create { name, role } => println!("{}", db.create_api_token(&name, role)?),
                TokenCommand::List => println!("{}", serde_json::to_string_pretty(&db.api_tokens()?)?),
                TokenCommand::Revoke { name } => db.revoke_api_token(&name)?,
            }
            Ok(())
        }
    }
}

async fn serve(cli: Serve, data_dir: PathBuf) -> Result<()> {
    if let Some(t) = &cli.admin_token {
        anyhow::ensure!(t.len() >= 16, "VIA_ADMIN_TOKEN must be at least 16 characters");
    }
    for k in &cli.trusted_keys {
        via_bundle::PublicKey::from_hex(k).with_context(|| format!("VIA_TRUSTED_KEYS: {k:?} is not a public key"))?;
    }
    let public_url = cli
        .public_url
        .context("VIA_PUBLIC_URL is required: the address devices reach this server at")?
        .trim_end_matches('/')
        .to_string();
    let device_url = if let Some(rest) = public_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = public_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("VIA_PUBLIC_URL must start with http:// or https://");
    };
    let db = Db::open(&data_dir.join("via.db"))?;
    if cli.admin_token.is_none() && db.api_tokens()?.is_empty() {
        tracing::warn!("no API tokens yet: create one with `via-server token create <name>`");
    }
    let state = Arc::new(State {
        db,
        hub: Hub::default(),
        images_dir: cli.images_dir.unwrap_or_else(|| data_dir.join("base")),
        os_dir: cli.os_dir.unwrap_or_else(|| data_dir.join("os")),
        data_dir,
        public_url,
        device_url,
        trusted_keys: cli.trusted_keys,
        admin_token_hash: cli.admin_token.as_deref().map(db::hash),
    });

    // MCP checks roles per tool; everything else here is admin-only.
    let people = Router::new()
        .nest_service("/mcp", mcp::service(state.clone()))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state.clone(), require_caller));
    let admin = Router::new()
        .route("/v1/releases/{app}/{release}/{arch}", axum::routing::put(upload_release))
        .route("/v1/os/{arch}/{version}/{file}", axum::routing::put(upload_os_file))
        .route("/v1/base/{arch}", axum::routing::put(upload_base))
        .route("/v1/devices/{device}/tunnel/{port}", get(tunnel))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_admin))
        .layer(middleware::from_fn_with_state(state.clone(), require_caller));
    let app = Router::new()
        .route("/v1/enroll", post(enroll))
        .route("/v1/connect", get(connect))
        .route("/v1/images/{id}", get(download))
        .route("/v1/os/{arch}/{version}/{file}", get(os_file))
        .route("/v1/releases/{app}/{release}/{arch}", get(release_bundle))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state.clone())
        .merge(people)
        .merge(admin);

    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    tracing::info!("listening on {} as {}", cli.listen, state.public_url);
    axum::serve(listener, app).await.context("server stopped")
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// Authenticate an API token and attach the [`db::Caller`] to the request.
async fn require_caller(AxState(s): AxState<Arc<State>>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return (StatusCode::UNAUTHORIZED, "API token required").into_response();
    };
    let caller = if s.admin_token_hash.as_deref() == Some(db::hash(token).as_str()) {
        Some(db::Caller { name: "admin".into(), role: db::Role::Admin })
    } else {
        s.db.api_caller(token).ok().flatten()
    };
    match caller {
        Some(c) => {
            req.extensions_mut().insert(c);
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, "unknown API token").into_response(),
    }
}

async fn require_admin(req: Request, next: Next) -> Response {
    match req.extensions().get::<db::Caller>() {
        Some(c) if c.role == db::Role::Admin => next.run(req).await,
        _ => (StatusCode::FORBIDDEN, "admin role required").into_response(),
    }
}

async fn enroll(AxState(s): AxState<Arc<State>>, Json(e): Json<db::Enrollment>) -> Response {
    match s.db.enroll(&e) {
        Ok((device, credential, image)) => {
            tracing::info!(device = %device.id, name = %device.name, fleet = %device.fleet, "enrolled");
            // The image held a token that is now spent; it can enroll nothing
            // else, and it is gigabytes.
            if let Some(id) = image {
                let _ = std::fs::remove_file(s.out_dir().join(format!("{id}.raw.zst")));
                let _ = std::fs::remove_file(s.out_dir().join(format!("{id}.name")));
            }
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

/// Update files are served to enrolled devices only. Names are checked
/// against the version's manifest, so no path from a request reaches the
/// filesystem unvetted.
async fn os_file(
    AxState(s): AxState<Arc<State>>,
    headers: HeaderMap,
    Path((arch, version, file)): Path<(String, String, String)>,
) -> Response {
    if !matches!(bearer(&headers).map(|c| s.db.authenticate(c)), Some(Ok(Some(_)))) {
        return (StatusCode::UNAUTHORIZED, "unknown device credential").into_response();
    }
    let Ok(release) = os::find(&s.os_dir, &arch, &version) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let known = file == "manifest.json" || file == "manifest.json.sig" || release.files.iter().any(|f| f == &file);
    if !known {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_file(&release.dir.join(&file), &file, "application/octet-stream").await
}

/// Stream a request body to `path`, hashing it on the way, and only then
/// make it visible: a half-uploaded file is never offered to anyone.
async fn save_body(path: &std::path::Path, body: Body) -> Result<String> {
    use futures_util::StreamExt;
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;

    tokio::fs::create_dir_all(path.parent().context("no parent directory")?).await?;
    let tmp = path.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut hash = sha2::Sha256::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(hex::encode(hash.finalize()))
}

fn uploaded(result: Result<serde_json::Value>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn upload_release(
    AxState(s): AxState<Arc<State>>,
    Extension(caller): Extension<db::Caller>,
    Path((app, release, arch)): Path<(String, u64, String)>,
    body: Body,
) -> Response {
    let result = async {
        let path = releases::bundle_path(&s.releases_dir(), &app, release, &arch)?;
        anyhow::ensure!(!path.exists(), "{app} release {release} for {arch} already exists; releases are immutable");
        let digest = save_body(&path.with_extension("upload"), body).await?;
        tokio::fs::write(path.with_extension("sha256"), &digest).await?;
        tokio::fs::rename(path.with_extension("upload"), &path).await?;
        Ok(json!({ "app": app, "release": release, "arch": arch, "sha256": digest }))
    }
    .await;
    s.db.audit(&caller.name, "upload_release", &json!({ "app": app, "release": release, "arch": arch }), result.is_ok());
    uploaded(result)
}

/// Upload one file of an OS version. `manifest.json` goes last: it is what
/// makes the version exist, so it is accepted only once every file it names
/// is present at the size it states.
async fn upload_os_file(
    AxState(s): AxState<Arc<State>>,
    Extension(caller): Extension<db::Caller>,
    Path((arch, version, file)): Path<(String, String, String)>,
    body: Body,
) -> Response {
    let result = async {
        let dir = os::upload_dir(&s.os_dir, &arch, &version, &file)?;
        if file == "manifest.json" {
            let staged = dir.join("manifest.json.upload");
            save_body(&staged, body).await?;
            if let Err(e) = os::check_complete(&dir, &staged, &arch, &version) {
                let _ = tokio::fs::remove_file(&staged).await;
                return Err(e);
            }
            tokio::fs::rename(&staged, dir.join("manifest.json")).await?;
            Ok(json!({ "arch": arch, "version": version, "published": true }))
        } else {
            let digest = save_body(&dir.join(&file), body).await?;
            Ok(json!({ "file": file, "sha256": digest }))
        }
    }
    .await;
    if file == "manifest.json" {
        s.db.audit(&caller.name, "upload_os", &json!({ "arch": arch, "version": version }), result.is_ok());
    }
    uploaded(result)
}

/// Replace the base image provisioning images are made from. The body is
/// the zstd-compressed raw image, as `via base-image` sends it.
async fn upload_base(
    AxState(s): AxState<Arc<State>>,
    Extension(caller): Extension<db::Caller>,
    Path(arch): Path<String>,
    body: Body,
) -> Response {
    let result = async {
        anyhow::ensure!(matches!(arch.as_str(), "x86_64" | "aarch64"), "arch is x86_64 or aarch64");
        let compressed = s.images_dir.join(format!("viaos-{arch}.raw.zst"));
        let digest = save_body(&compressed, body).await?;
        let raw = s.images_dir.join(format!("viaos-{arch}.raw"));
        let c = compressed.clone();
        tokio::task::spawn_blocking(move || image::decompress(&c, &raw)).await??;
        let _ = tokio::fs::remove_file(&compressed).await;
        Ok(json!({ "arch": arch, "sha256_compressed": digest }))
    }
    .await;
    s.db.audit(&caller.name, "upload_base", &json!({ "arch": arch }), result.is_ok());
    uploaded(result)
}

/// A TCP stream to `port` on a device, carried over the device's own
/// connection: SSH to a device behind NAT is `ssh -o ProxyCommand='via
/// tunnel %h 22'`. The WebSocket's binary messages are the stream's bytes.
async fn tunnel(
    AxState(s): AxState<Arc<State>>,
    Extension(caller): Extension<db::Caller>,
    Path((device, port)): Path<(String, u16)>,
    ws: WebSocketUpgrade,
) -> Response {
    let opened = s.db.resolve(&device).and_then(|d| s.hub.open(&d.id, &format!("tcp:{port}")));
    s.db.audit(&caller.name, "tunnel", &json!({ "device": device, "port": port }), opened.is_ok());
    match opened {
        Ok(stream) => ws.on_upgrade(move |socket| bridge(socket, stream)),
        Err(e) => (StatusCode::NOT_FOUND, format!("{e:#}")).into_response(),
    }
}

async fn bridge(socket: axum::extract::ws::WebSocket, stream: tokio::io::DuplexStream) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut r, mut w) = tokio::io::split(stream);
    let up = async {
        while let Some(Ok(m)) = ws_rx.next().await {
            match m {
                Message::Binary(b) => {
                    if w.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = w.shutdown().await;
    };
    let down = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    };
    tokio::join!(up, down);
}

async fn release_bundle(
    AxState(s): AxState<Arc<State>>,
    headers: HeaderMap,
    Path((app, release, arch)): Path<(String, u64, String)>,
) -> Response {
    if !matches!(bearer(&headers).map(|c| s.db.authenticate(c)), Some(Ok(Some(_)))) {
        return (StatusCode::UNAUTHORIZED, "unknown device credential").into_response();
    }
    match releases::bundle_path(&s.releases_dir(), &app, release, &arch) {
        Ok(p) => serve_file(&p, &format!("{app}-{release}-{arch}.bundle"), "application/octet-stream").await,
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_file(path: &std::path::Path, name: &str, content_type: &str) -> Response {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        body,
    )
        .into_response()
}

/// The id is 96 random bits and the only way to name the file, so the link
/// itself is the credential. It is shown to whoever asked for the image.
async fn download(AxState(s): AxState<Arc<State>>, Path(id): Path<String>) -> Response {
    if id.len() != 24 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let name = std::fs::read_to_string(s.out_dir().join(format!("{id}.name")))
        .unwrap_or_else(|_| "viaos.raw.zst".into());
    serve_file(&s.out_dir().join(format!("{id}.raw.zst")), &name, "application/zstd").await
}
