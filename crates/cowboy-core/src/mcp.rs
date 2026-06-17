//! MCP (Model Context Protocol) server configuration.
//!
//! Cowboy can connect to external MCP servers the **user** configures and let the
//! agent discover and call their tools. MCP servers are **host-side trusted
//! integrations**: the config is host-owned (`~/.config/cowboy/mcp.yaml`, never
//! mounted into the agent container), the agent can *call* configured servers but
//! never add/edit them, and their traffic runs on the host — outside the agent's
//! container gateway, exactly like credential grants. Configuring a server *is* the
//! trust gate.
//!
//! This module is config-only (serde types + load/save). The live client (connect,
//! `tools/list`, `tools/call`) lives host-side in `cowboy-cli` (it needs the MCP
//! SDK + process/HTTP transports).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

fn default_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

/// The set of configured MCP servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Servers keyed by a short local name (e.g. `linear`, `filesystem`).
    #[serde(default)]
    pub servers: BTreeMap<String, McpServer>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            servers: BTreeMap::new(),
        }
    }
}

/// One configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// A one-line summary shown to the agent so it knows what the server is for
    /// (e.g. "issue tracking", "company docs search"). Keep it short.
    #[serde(default)]
    pub description: String,
    /// Disabled servers are ignored (not connected, not shown to the agent).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How to reach the server.
    pub transport: McpTransport,
    /// Optional allowlist of tool names to expose. Empty = expose all the server's
    /// tools. Use it to keep a chatty server's surface focused.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// MCP transport: a local stdio subprocess, or a remote streamable-HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    /// A local subprocess speaking MCP over stdio.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment for the subprocess. Values may reference host env with
        /// `${VAR}` (expanded host-side); never inline secret literals.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// A remote server reachable over streamable HTTP / SSE.
    Http {
        url: String,
        /// Request headers (e.g. `Authorization: Bearer ${TOKEN}`). Values may
        /// reference host env with `${VAR}`; never inline secret literals.
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl McpServer {
    /// Is this server a local stdio subprocess?
    pub fn is_stdio(&self) -> bool {
        matches!(self.transport, McpTransport::Stdio { .. })
    }

    /// A short human label for the transport (for `cowboy mcp list`).
    pub fn transport_label(&self) -> String {
        match &self.transport {
            McpTransport::Stdio { command, .. } => format!("stdio: {command}"),
            McpTransport::Http { url, .. } => format!("http: {url}"),
        }
    }
}

/// Expand `${VAR}` references in `s` from the host environment. An unset variable
/// expands to the empty string (the server simply sees a blank value). `$$` is a
/// literal `$`. Resolution is host-side only — values never touch the container.
pub fn expand_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some((_, '$')) => {
                chars.next();
                out.push('$');
            }
            Some((_, '{')) => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                for (_, nc) in chars.by_ref() {
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // Unterminated `${…` — emit verbatim rather than silently drop.
                    out.push_str("${");
                    out.push_str(&name);
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

/// Expand `${VAR}` in every value of a map (keys are left as-is).
pub fn expand_map(map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    map.iter()
        .map(|(k, v)| (k.clone(), expand_vars(v)))
        .collect()
}

/// The host-owned config file (`~/.config/cowboy/mcp.yaml`). Like
/// `crew.yaml`/`providers.yaml`: never mounted into the container.
pub fn path() -> Option<PathBuf> {
    crate::config::global_config_dir().map(|d| d.join("mcp.yaml"))
}

/// Load the config, or `None` if no file exists yet.
pub fn load() -> Result<Option<McpConfig>> {
    let Some(p) = path() else { return Ok(None) };
    if !p.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&p).map_err(|e| Error::Invalid(e.to_string()))?;
    serde_yaml_ng::from_str(&text)
        .map(Some)
        .map_err(|e| Error::Invalid(format!("parsing mcp.yaml: {e}")))
}

/// Load the config or an empty default (so callers don't special-case "no file").
pub fn load_or_default() -> Result<McpConfig> {
    Ok(load()?.unwrap_or_default())
}

/// Write the config (creates `~/.config/cowboy/`; atomic temp+rename).
pub fn save(cfg: &McpConfig) -> Result<()> {
    let p = path().ok_or_else(|| Error::Invalid("cannot resolve home config dir".into()))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    let yaml = serde_yaml_ng::to_string(cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = p.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

impl McpConfig {
    /// Enabled servers, in name order.
    pub fn enabled(&self) -> impl Iterator<Item = (&String, &McpServer)> {
        self.servers.iter().filter(|(_, s)| s.enabled)
    }

    /// Whether any server is enabled (gates the agent's `mcp` tool + prompt block).
    pub fn any_enabled(&self) -> bool {
        self.servers.values().any(|s| s.enabled)
    }
}

// ---------------------------------------------------------------------------
// Project `.mcp.json` (the Claude Code convention)
// ---------------------------------------------------------------------------

/// The path to a repo's project MCP file (`<root>/.mcp.json`).
pub fn project_mcp_json_path(root: &Path) -> PathBuf {
    root.join(".mcp.json")
}

/// Parse a repo's `.mcp.json` into our server map. Mirrors the Claude Code schema:
/// `{ "mcpServers": { "<name>": { … } } }`, where each entry is a local stdio
/// subprocess (`command`/`args`/`env`, the default) or a remote endpoint
/// (`"type": "http" | "sse"` with `url`/`headers`). `${VAR}` in env/header values
/// is expanded host-side at connect time (same as host config). These servers are
/// **trust-gated** — parsing does not imply they will be used.
pub fn parse_mcp_json(text: &str) -> Result<BTreeMap<String, McpServer>> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default, rename = "mcpServers")]
        mcp_servers: BTreeMap<String, RawServer>,
    }
    #[derive(Deserialize)]
    struct RawServer {
        #[serde(rename = "type")]
        kind: Option<String>,
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        url: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        description: Option<String>,
    }

    let file: File = serde_json::from_str(text)
        .map_err(|e| Error::Invalid(format!("parsing .mcp.json: {e}")))?;
    let mut out = BTreeMap::new();
    for (name, raw) in file.mcp_servers {
        let transport = match raw.kind.as_deref() {
            Some("http") | Some("sse") => {
                let url = raw
                    .url
                    .ok_or_else(|| Error::Invalid(format!("server `{name}`: `url` is required")))?;
                McpTransport::Http {
                    url,
                    headers: raw.headers,
                }
            }
            Some("stdio") | None => {
                // A `url`-only entry with no command is treated as remote even
                // without an explicit `type` (lenient, matches common files).
                match (raw.command, raw.url) {
                    (None, Some(url)) => McpTransport::Http {
                        url,
                        headers: raw.headers,
                    },
                    (Some(command), _) => McpTransport::Stdio {
                        command,
                        args: raw.args,
                        env: raw.env,
                    },
                    (None, None) => {
                        return Err(Error::Invalid(format!(
                            "server `{name}`: `command` is required"
                        )))
                    }
                }
            }
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "server `{name}`: unknown type `{other}` (expected stdio|http|sse)"
                )))
            }
        };
        out.insert(
            name,
            McpServer {
                description: raw.description.unwrap_or_else(|| "(from .mcp.json)".into()),
                enabled: true,
                transport,
                tools: vec![],
            },
        );
    }
    Ok(out)
}

/// Load a repo's project servers, or `None` if it has no `.mcp.json`.
pub fn load_project_mcp(root: &Path) -> Result<Option<BTreeMap<String, McpServer>>> {
    let p = project_mcp_json_path(root);
    if !p.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&p).map_err(|e| Error::Invalid(e.to_string()))?;
    parse_mcp_json(&text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips_through_yaml() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "filesystem".to_string(),
            McpServer {
                description: "local files".into(),
                enabled: true,
                transport: McpTransport::Stdio {
                    command: "npx".into(),
                    args: vec!["-y".into(), "@mcp/fs".into(), "/workspace".into()],
                    env: BTreeMap::new(),
                },
                tools: vec![],
            },
        );
        servers.insert(
            "linear".to_string(),
            McpServer {
                description: "issue tracking".into(),
                enabled: false,
                transport: McpTransport::Http {
                    url: "https://mcp.linear.app/sse".into(),
                    headers: BTreeMap::from([(
                        "Authorization".into(),
                        "Bearer ${LINEAR_TOKEN}".into(),
                    )]),
                },
                tools: vec!["create_issue".into()],
            },
        );
        let cfg = McpConfig {
            version: 1,
            servers,
        };
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let back: McpConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(cfg, back);
        // Only `filesystem` is enabled.
        assert!(cfg.any_enabled());
        assert_eq!(cfg.enabled().count(), 1);
    }

    #[test]
    fn missing_optional_fields_default() {
        let yaml = "
servers:
  fs:
    transport:
      type: stdio
      command: server-fs
";
        let cfg: McpConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let s = &cfg.servers["fs"];
        assert!(s.enabled, "enabled defaults to true");
        assert!(s.tools.is_empty());
        assert_eq!(s.description, "");
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn parse_mcp_json_handles_stdio_http_and_sse() {
        let text = r#"
        {
          "mcpServers": {
            "fs":    { "command": "npx", "args": ["-y", "@mcp/fs"], "env": { "X": "${X}" } },
            "linear":{ "type": "sse", "url": "https://mcp.linear.app/sse",
                       "headers": { "Authorization": "Bearer ${T}" } },
            "api":   { "type": "http", "url": "https://example.com/mcp" },
            "bare":  { "url": "https://bare.example/mcp" }
          }
        }
        "#;
        let servers = parse_mcp_json(text).unwrap();
        assert_eq!(servers.len(), 4);
        match &servers["fs"].transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "@mcp/fs"]);
                assert_eq!(env["X"], "${X}"); // raw; expanded at connect time
            }
            _ => panic!("fs should be stdio"),
        }
        // sse and a bare url-only entry both map to our Http transport.
        assert!(matches!(
            servers["linear"].transport,
            McpTransport::Http { .. }
        ));
        assert!(matches!(
            servers["api"].transport,
            McpTransport::Http { .. }
        ));
        assert!(matches!(
            servers["bare"].transport,
            McpTransport::Http { .. }
        ));
        assert!(servers["fs"].enabled);
    }

    #[test]
    fn parse_mcp_json_rejects_bad_entries() {
        // stdio with no command.
        assert!(parse_mcp_json(r#"{"mcpServers":{"x":{"args":["a"]}}}"#).is_err());
        // http with no url.
        assert!(parse_mcp_json(r#"{"mcpServers":{"x":{"type":"http"}}}"#).is_err());
        // unknown type.
        assert!(parse_mcp_json(r#"{"mcpServers":{"x":{"type":"ftp","url":"u"}}}"#).is_err());
        // malformed json.
        assert!(parse_mcp_json("not json").is_err());
        // empty / no servers is fine.
        assert_eq!(parse_mcp_json("{}").unwrap().len(), 0);
    }

    #[test]
    fn expand_vars_handles_set_unset_and_literals() {
        std::env::set_var("MCP_TEST_TOKEN", "secret123");
        assert_eq!(expand_vars("Bearer ${MCP_TEST_TOKEN}"), "Bearer secret123");
        // Unset → empty.
        assert_eq!(expand_vars("x=${MCP_TEST_UNSET_XYZ}!"), "x=!");
        // `$$` is a literal dollar; bare `$` and unterminated `${` are preserved.
        assert_eq!(expand_vars("cost $$5"), "cost $5");
        assert_eq!(expand_vars("a $ b"), "a $ b");
        assert_eq!(expand_vars("${unterminated"), "${unterminated");
        std::env::remove_var("MCP_TEST_TOKEN");
    }
}
