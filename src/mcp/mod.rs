//! Minimal MCP (Model Context Protocol) client.
//!
//! Hand-rolled because the abstraction we need is small:
//! - spawn a server as a child process
//! - exchange JSON-RPC 2.0 over its stdio (newline-delimited)
//! - implement just `initialize`, `tools/list`, `tools/call`
//!
//! Each MCP server's tools are wrapped as `McpTool` and registered into the
//! same `ToolRegistry` as built-in tools; the agent doesn't see the
//! difference.
//!
//! Tool name namespacing: `<server>_<tool>` — OpenAI tool names must match
//! `^[a-zA-Z0-9_-]{1,64}$` (no dots).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::llm::ToolSchema;
use crate::tool::{Tool, ToolContext};

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

pub struct McpClient {
    name: String,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    writer_tx: mpsc::Sender<Vec<u8>>,
    _child: Mutex<Child>, // hold to keep server alive; Drop kills it
}

impl McpClient {
    pub async fn spawn(cfg: &McpServerConfig) -> Result<Arc<Self>> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn MCP server '{}': {}", cfg.name, cfg.command))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Default::default();

        // Writer task
        let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(32);
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(bytes) = writer_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });

        // Reader task
        let pending_clone = pending.clone();
        let server_name = cfg.name.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("MCP[{}] non-JSON line: {} ({})", server_name, trimmed, e);
                        continue;
                    }
                };
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    if let Some(sender) = pending_clone.lock().await.remove(&id) {
                        let _ = sender.send(v);
                    }
                }
                // notifications (no id) — ignored for now
            }
        });

        let client = Arc::new(Self {
            name: cfg.name.clone(),
            next_id: AtomicU64::new(1),
            pending,
            writer_tx,
            _child: Mutex::new(child),
        });

        // Handshake
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "folkbot", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
            .context("MCP initialize")?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;

        Ok(client)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let bytes = serde_json::to_vec(&req)?;
        self.writer_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow!("MCP writer closed"))?;

        let resp = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("MCP[{}] request {} timed out", self.name, method))?
            .map_err(|_| anyhow!("MCP[{}] reader dropped", self.name))?;

        if let Some(err) = resp.get("error") {
            bail!("MCP[{}] error on {}: {}", self.name, method, err);
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let bytes = serde_json::to_vec(&req)?;
        self.writer_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow!("MCP writer closed"))?;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools_v = result.get("tools").cloned().unwrap_or(Value::Array(vec![]));
        let descriptors: Vec<McpToolDescriptor> = serde_json::from_value(tools_v)
            .with_context(|| format!("MCP[{}] parse tools list", self.name))?;
        Ok(descriptors)
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": args,
                }),
            )
            .await?;
        Ok(result)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_schema", rename = "inputSchema")]
    pub input_schema: Value,
}

fn default_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

pub struct McpTool {
    server: String,
    inner_name: String,
    descriptor: McpToolDescriptor,
    client: Arc<McpClient>,
}

impl McpTool {
    pub fn new(server: String, descriptor: McpToolDescriptor, client: Arc<McpClient>) -> Self {
        let inner = descriptor.name.clone();
        Self { server, inner_name: inner, descriptor, client }
    }

    pub fn exposed_name(&self) -> String {
        // OpenAI tool names: ^[a-zA-Z0-9_-]{1,64}$
        let combined = format!("{}_{}", self.server, self.inner_name);
        let cleaned: String = combined
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        if cleaned.len() > 64 {
            cleaned[..64].to_string()
        } else {
            cleaned
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.exposed_name(),
            description: self.descriptor.description.clone(),
            parameters: self.descriptor.input_schema.clone(),
        }
    }

    async fn invoke(&self, args: Value, _ctx: &ToolContext) -> Result<Value> {
        self.client.call_tool(&self.inner_name, args).await
    }
}
