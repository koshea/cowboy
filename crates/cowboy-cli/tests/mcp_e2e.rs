//! End-to-end test for the host-side MCP client manager.
//!
//! `#[ignore]`: needs `npx` (Node) and network to fetch the reference MCP server
//! `@modelcontextprotocol/server-everything`. Run explicitly:
//!
//! ```text
//! cargo test -p cowboy-cli --test mcp_e2e -- --ignored
//! ```
//!
//! Self-skips (prints why, returns) when `npx` is missing, so `--ignored` is safe
//! to run anywhere. Exercises the real stdio transport: connect + initialize,
//! `tools/list` (with schemas), and `tools/call`.

use std::collections::BTreeMap;

use cowboy_cli::mcp::McpManager;
use cowboy_core::mcp::{McpConfig, McpServer, McpTransport};

fn npx_available() -> bool {
    std::process::Command::new("npx")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn everything_config() -> McpConfig {
    let mut servers = BTreeMap::new();
    servers.insert(
        "everything".to_string(),
        McpServer {
            description: "reference MCP server".into(),
            enabled: true,
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    "@modelcontextprotocol/server-everything".into(),
                ],
                env: BTreeMap::new(),
            },
            tools: vec![],
        },
    );
    McpConfig {
        version: 1,
        servers,
    }
}

#[tokio::test]
#[ignore = "needs npx + network: connects to a real stdio MCP server"]
async fn mcp_manager_lists_and_calls_a_real_stdio_server() {
    if !npx_available() {
        eprintln!("skipping: npx not available");
        return;
    }
    let mgr = McpManager::new(everything_config());

    // Discovery: the reference server exposes an `echo` tool (with a schema).
    let listing = mgr
        .list_tools(Some("everything"))
        .await
        .expect("list_tools should succeed");
    assert!(
        listing.contains("echo"),
        "expected an `echo` tool in the listing:\n{listing}"
    );
    assert!(
        listing.contains("input schema"),
        "discovery should include tool input schemas"
    );

    // Call: `echo` returns the message back.
    let result = mgr
        .call_tool(
            "everything",
            "echo",
            Some(serde_json::json!({ "message": "cowboy-mcp-ok" })),
        )
        .await
        .expect("call_tool should succeed");
    assert!(
        result.contains("cowboy-mcp-ok"),
        "echo should return the message, got:\n{result}"
    );
}

#[tokio::test]
#[ignore = "needs npx + network: connects to a real stdio MCP server"]
async fn project_mcp_json_server_parses_and_connects() {
    if !npx_available() {
        eprintln!("skipping: npx not available");
        return;
    }
    // A repo with a .mcp.json declaring the reference stdio server.
    let repo = assert_fs::TempDir::new().unwrap();
    std::fs::write(
        repo.path().join(".mcp.json"),
        r#"{ "mcpServers": { "everything": {
              "command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"] } } }"#,
    )
    .unwrap();

    // Parse the project file → our server map, build a manager, connect + list.
    let servers = cowboy_core::mcp::load_project_mcp(repo.path())
        .expect("load_project_mcp")
        .expect("a .mcp.json is present");
    assert!(servers.contains_key("everything"));
    let mgr = McpManager::new(McpConfig {
        version: 1,
        servers,
    });
    let listing = mgr
        .list_tools(Some("everything"))
        .await
        .expect("connect + list");
    assert!(
        listing.contains("echo"),
        "expected the project server's tools, got:\n{listing}"
    );
}

#[tokio::test]
#[ignore = "needs npx + network: connects to a real stdio MCP server"]
async fn mcp_manager_enforces_the_tool_allowlist() {
    if !npx_available() {
        eprintln!("skipping: npx not available");
        return;
    }
    let mut cfg = everything_config();
    // Restrict to a tool that is NOT `echo`.
    cfg.servers.get_mut("everything").unwrap().tools = vec!["add".into()];
    let mgr = McpManager::new(cfg);

    // `echo` is now outside the allowlist → rejected without even calling it.
    let err = mgr
        .call_tool(
            "everything",
            "echo",
            Some(serde_json::json!({"message": "x"})),
        )
        .await
        .expect_err("echo should be blocked by the allowlist");
    assert!(
        err.to_string().contains("allowlist"),
        "expected an allowlist rejection, got: {err}"
    );
}
