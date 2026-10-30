//! Devices that are connected right now, and how to reach them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use via_link::{Mux, Side};

/// How long a forwarded tool call may take before the caller gets an error.
/// Long work belongs in jobs, not in one call held open across the network.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
pub struct Hub {
    // A connection generation travels with each mux, so a device that
    // reconnects is not unregistered by its old connection closing late.
    online: Arc<Mutex<HashMap<String, (u64, Mux)>>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
}

impl Hub {
    pub fn is_online(&self, device: &str) -> bool {
        self.online.lock().unwrap().contains_key(device)
    }

    /// Serve one device's connection until it closes.
    pub async fn attach(&self, device: String, socket: WebSocket) {
        let (mux, mut outgoing, _incoming) = Mux::new(Side::Server);
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.online.lock().unwrap().insert(device.clone(), (generation, mux.clone()));
        tracing::info!(%device, "connected");

        let (mut tx, mut rx) = socket.split();
        let send = tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if tx.send(Message::Binary(frame.into())).await.is_err() {
                    break;
                }
            }
        });
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Binary(b) => mux.receive(&b),
                Message::Close(_) => break,
                _ => {}
            }
        }
        send.abort();

        let mut online = self.online.lock().unwrap();
        if online.get(&device).is_some_and(|(g, _)| *g == generation) {
            online.remove(&device);
        }
        tracing::info!(%device, "disconnected");
    }

    /// Call one of a device's own MCP tools.
    pub async fn call(&self, device: &str, tool: &str, args: JsonObject) -> Result<CallToolResult> {
        let mux = self
            .online
            .lock()
            .unwrap()
            .get(device)
            .map(|(_, m)| m.clone())
            .context("device is offline")?;
        let call = async {
            let client = ().serve(mux.open("mcp")).await.context("starting MCP session")?;
            let result = client
                .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
                .await
                .context("tool call failed");
            let _ = client.cancel().await;
            result
        };
        tokio::time::timeout(CALL_TIMEOUT, call).await.context("device did not answer in time")?
    }
}
