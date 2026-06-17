//! Host-side MCP client manager.
//!
//! Connects to the user's configured MCP servers (host-owned `mcp.yaml`) and lets
//! the agent discover + call their tools. MCP servers are **host-side trusted
//! integrations**: they run on the host (outside the agent container and its
//! network gateway), the agent can only call configured servers, and credentials
//! stay host-owned. See [`cowboy_core::mcp`] for the config + trust model.
//!
//! The manager keeps one lazily-established connection per server for the life of
//! the session (cached), and exposes two operations the agent's `mcp` tool drives:
//! `list_tools` (discovery — full schemas on demand) and `call_tool`.

pub mod trust;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use cowboy_core::mcp::{self, McpConfig, McpServer, McpTransport};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::Mutex;

/// A live client connection to one MCP server (the `()` client handler — we only
/// initiate requests, we don't serve callbacks).
type Client = RunningService<RoleClient, ()>;

/// How long to wait for a server to connect/initialize before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Manages connections to the configured MCP servers for one session.
pub struct McpManager {
    config: McpConfig,
    clients: Mutex<HashMap<String, Arc<Client>>>,
}

impl McpManager {
    pub fn new(config: McpConfig) -> Self {
        Self {
            config,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The enabled servers as `(name, description)`, in name order — for the prompt
    /// block and `/mcp`. No connection is made (cheap, always available).
    pub fn connected_servers(&self) -> Vec<(String, String)> {
        self.config
            .enabled()
            .map(|(n, s)| (n.clone(), s.description.clone()))
            .collect()
    }

    /// Look up an enabled server by name.
    fn server(&self, name: &str) -> Result<&McpServer> {
        match self.config.servers.get(name) {
            Some(s) if s.enabled => Ok(s),
            Some(_) => Err(anyhow!("MCP server `{name}` is disabled")),
            None => Err(anyhow!(
                "no MCP server `{name}` (configured: {})",
                self.server_names().join(", ")
            )),
        }
    }

    fn server_names(&self) -> Vec<String> {
        self.config.enabled().map(|(n, _)| n.clone()).collect()
    }

    /// Get-or-create the live connection for a server (cached for the session).
    async fn connect(&self, name: &str) -> Result<Arc<Client>> {
        if let Some(c) = self.clients.lock().await.get(name) {
            return Ok(c.clone());
        }
        let server = self.server(name)?.clone();
        let client = self.dial(name, &server).await?;
        let client = Arc::new(client);
        self.clients
            .lock()
            .await
            .insert(name.to_string(), client.clone());
        Ok(client)
    }

    /// Establish a fresh connection over the server's configured transport.
    async fn dial(&self, name: &str, server: &McpServer) -> Result<Client> {
        let connect = async {
            match &server.transport {
                McpTransport::Stdio { command, args, env } => {
                    let args = args.clone();
                    let env = mcp::expand_map(env);
                    let cmd = command.clone();
                    let configured = tokio::process::Command::new(&cmd).configure(|c| {
                        c.args(&args);
                        for (k, v) in &env {
                            c.env(k, v);
                        }
                    });
                    // Discard the server's stderr so its own logging doesn't leak
                    // into our output (stdin/stdout carry the MCP protocol).
                    let (transport, _stderr) = TokioChildProcess::builder(configured)
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .map_err(|e| anyhow!("spawning `{cmd}`: {e}"))?;
                    ().serve(transport)
                        .await
                        .map_err(|e| anyhow!("MCP handshake failed: {e}"))
                }
                McpTransport::Http { url, headers } => {
                    let headers = mcp::expand_map(headers);
                    let transport = http_transport(url, &headers)?;
                    ().serve(transport)
                        .await
                        .map_err(|e| anyhow!("MCP handshake failed: {e}"))
                }
            }
        };
        tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| anyhow!("timed out connecting to MCP server `{name}`"))?
    }

    /// Discover tools. With `server`, returns that server's tools **with full
    /// schemas** (the on-demand cost). Without, returns a compact tool-name listing
    /// across every enabled server. The result is the observation the agent reads.
    pub async fn list_tools(&self, server: Option<&str>) -> Result<String> {
        match server {
            Some(name) => self.list_one(name).await,
            None => {
                let mut out = String::new();
                for n in self.server_names() {
                    match self.list_one_compact(&n).await {
                        Ok(line) => out.push_str(&line),
                        Err(e) => out.push_str(&format!("## {n}\n  (unreachable: {e})\n")),
                    }
                }
                if out.is_empty() {
                    out.push_str("no MCP servers are enabled.");
                }
                Ok(out)
            }
        }
    }

    /// Full tool list + JSON schemas for one server (filtered by its allowlist).
    async fn list_one(&self, name: &str) -> Result<String> {
        let allow = self.server(name)?.tools.clone();
        let client = self.connect(name).await?;
        let tools = client
            .list_all_tools()
            .await
            .map_err(|e| anyhow!("listing tools on `{name}`: {e}"))?;
        let mut out = format!("# MCP server `{name}` tools\n");
        let mut shown = 0;
        for t in &tools {
            if !allow.is_empty() && !allow.iter().any(|a| a == t.name.as_ref()) {
                continue;
            }
            shown += 1;
            out.push_str(&format!("\n## {}\n", t.name));
            if let Some(d) = &t.description {
                out.push_str(&format!("{d}\n"));
            }
            let schema =
                serde_json::to_string_pretty(&*t.input_schema).unwrap_or_else(|_| "{}".into());
            out.push_str(&format!("input schema:\n{schema}\n"));
        }
        if shown == 0 {
            out.push_str("\n(no tools exposed)\n");
        }
        out.push_str(&format!(
            "\nCall one with: mcp {{ \"action\": \"call\", \"server\": \"{name}\", \
             \"tool\": \"<name>\", \"arguments\": {{ … }} }}\n"
        ));
        Ok(out)
    }

    /// One-line-per-tool summary for a server (no schemas) — the cheap overview.
    async fn list_one_compact(&self, name: &str) -> Result<String> {
        let allow = self.server(name)?.tools.clone();
        let client = self.connect(name).await?;
        let tools = client
            .list_all_tools()
            .await
            .map_err(|e| anyhow!("listing tools on `{name}`: {e}"))?;
        let mut out = format!("## {name}\n");
        for t in &tools {
            if !allow.is_empty() && !allow.iter().any(|a| a == t.name.as_ref()) {
                continue;
            }
            let desc = t
                .description
                .as_deref()
                .map(|d| d.lines().next().unwrap_or("").trim())
                .unwrap_or("");
            out.push_str(&format!("  - {}: {desc}\n", t.name));
        }
        Ok(out)
    }

    /// Call a tool on a server and return its textual result as the observation.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String> {
        // Enforce the per-server allowlist (if any).
        let allow = self.server(server)?.tools.clone();
        if !allow.is_empty() && !allow.iter().any(|a| a == tool) {
            return Err(anyhow!(
                "tool `{tool}` is not in `{server}`'s allowlist ({})",
                allow.join(", ")
            ));
        }
        let arguments = match arguments {
            Some(serde_json::Value::Object(m)) => Some(m),
            Some(serde_json::Value::Null) | None => None,
            Some(other) => return Err(anyhow!("`arguments` must be a JSON object, got: {other}")),
        };
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let client = self.connect(server).await?;
        let result = client
            .call_tool(params)
            .await
            .map_err(|e| anyhow!("calling `{server}.{tool}`: {e}"))?;
        Ok(render_result(&result))
    }
}

/// Build a streamable-HTTP transport, applying any configured request headers.
fn http_transport(
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
    if headers.is_empty() {
        return Ok(StreamableHttpClientTransport::from_uri(url));
    }
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| anyhow!("invalid header name `{k}`: {e}"))?;
        let val = HeaderValue::from_str(v).map_err(|e| anyhow!("invalid header `{k}`: {e}"))?;
        map.insert(name, val);
    }
    let client = reqwest::Client::builder()
        .default_headers(map)
        .build()
        .map_err(|e| anyhow!("building HTTP client: {e}"))?;
    Ok(StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(url),
    ))
}

/// Flatten an MCP tool result into a text observation for the agent.
fn render_result(result: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in &result.content {
        if let Some(text) = c.as_text() {
            parts.push(text.text.clone());
        }
    }
    let mut body = if parts.is_empty() {
        // No text blocks — fall back to structured content if present.
        match &result.structured_content {
            Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
            None => "(tool returned no textual content)".to_string(),
        }
    } else {
        parts.join("\n")
    };
    if result.is_error.unwrap_or(false) {
        body = format!("[tool reported an error]\n{body}");
    }
    body
}
