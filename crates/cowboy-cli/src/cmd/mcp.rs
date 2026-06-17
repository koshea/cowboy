//! `cowboy mcp …` — manage the host-owned MCP server config
//! (`~/.config/cowboy/mcp.yaml`). MCP servers are trusted integrations the user
//! configures; the agent can call configured servers but never edit this file.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use cowboy_core::mcp::{self, McpConfig, McpServer, McpTransport};

use crate::cli::{McpAddArgs, McpCommand};

pub async fn run(command: McpCommand) -> Result<()> {
    match command {
        McpCommand::List => list(),
        McpCommand::Show { name } => show(&name),
        McpCommand::Add(args) => add(args),
        McpCommand::Remove { name } => remove(&name),
        McpCommand::Enable { name } => set_enabled(&name, true),
        McpCommand::Disable { name } => set_enabled(&name, false),
        McpCommand::Test { name } => test(&name).await,
        McpCommand::Trust => trust(),
        McpCommand::Untrust => untrust(),
    }
}

fn load() -> Result<McpConfig> {
    mcp::load_or_default().context("loading mcp.yaml")
}

fn save(cfg: &McpConfig) -> Result<()> {
    mcp::save(cfg).context("writing mcp.yaml")
}

fn list() -> Result<()> {
    let cfg = load()?;
    let host_empty = cfg.servers.is_empty();
    if host_empty {
        println!("no host MCP servers configured (~/.config/cowboy/mcp.yaml).");
        println!("add one: `cowboy mcp add <name> --transport stdio --command … [--arg …]`");
        println!("      or `cowboy mcp add <name> --transport http --url https://…`");
    } else {
        println!("Host servers (~/.config/cowboy/mcp.yaml):");
        for (name, s) in &cfg.servers {
            let state = if s.enabled { "enabled" } else { "disabled" };
            let allow = if s.tools.is_empty() {
                String::new()
            } else {
                format!("  tools: {}", s.tools.join(", "))
            };
            println!("  {name}  [{state}]  {}{allow}", s.transport_label());
            if !s.description.is_empty() {
                println!("      {}", s.description);
            }
        }
    }

    // Project servers from this repo's `.mcp.json` (trust-gated).
    let root = crate::cmd::project_root()?;
    let state = crate::mcp::trust::project_trust(&root);
    if state != crate::mcp::trust::TrustState::NoFile {
        if let Ok(Some(servers)) = cowboy_core::mcp::load_project_mcp(&root) {
            println!("\nProject servers (.mcp.json) — {}:", state.label());
            for (name, s) in &servers {
                let shadowed = cfg.servers.contains_key(name);
                let note = if shadowed {
                    "  (shadowed by host config)"
                } else {
                    ""
                };
                println!("  {name}  {}{note}", s.transport_label());
            }
            if state == crate::mcp::trust::TrustState::Untrusted
                || state == crate::mcp::trust::TrustState::Stale
            {
                println!("  → review and enable with `cowboy mcp trust`");
            }
        }
    }
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let cfg = load()?;
    let s = cfg
        .servers
        .get(name)
        .with_context(|| format!("no MCP server `{name}`"))?;
    // Render just this server as YAML.
    let yaml = serde_yaml_ng::to_string(s).context("rendering server")?;
    println!("# {name}\n{yaml}");
    Ok(())
}

fn add(args: McpAddArgs) -> Result<()> {
    let transport = match args.transport.as_str() {
        "stdio" => {
            let command = args
                .command
                .context("`--command` is required for a stdio server")?;
            McpTransport::Stdio {
                command,
                args: args.args,
                env: parse_kv(&args.env).context("parsing --env")?,
            }
        }
        "http" => {
            let url = args.url.context("`--url` is required for an http server")?;
            McpTransport::Http {
                url,
                headers: parse_kv(&args.header).context("parsing --header")?,
            }
        }
        other => bail!("unknown transport `{other}` (expected `stdio` or `http`)"),
    };
    let server = McpServer {
        description: args.description.unwrap_or_default(),
        enabled: true,
        transport,
        tools: args.tools,
    };
    let mut cfg = load()?;
    let existed = cfg.servers.insert(args.name.clone(), server).is_some();
    save(&cfg)?;
    let verb = if existed { "updated" } else { "added" };
    println!("✓ {verb} MCP server `{}`", args.name);
    println!("  check it with `cowboy mcp test {}`", args.name);
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let mut cfg = load()?;
    if cfg.servers.remove(name).is_none() {
        bail!("no MCP server `{name}`");
    }
    save(&cfg)?;
    println!("✓ removed MCP server `{name}`");
    Ok(())
}

fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    let mut cfg = load()?;
    let s = cfg
        .servers
        .get_mut(name)
        .with_context(|| format!("no MCP server `{name}`"))?;
    s.enabled = enabled;
    save(&cfg)?;
    println!(
        "✓ {} MCP server `{name}`",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

async fn test(name: &str) -> Result<()> {
    let mut cfg = load()?;
    // Also allow testing a trusted project (.mcp.json) server (host config wins).
    let root = crate::cmd::project_root()?;
    for (n, s) in crate::mcp::trust::trusted_servers(&root) {
        cfg.servers.entry(n).or_insert(s);
    }
    if !cfg.servers.contains_key(name) {
        bail!("no MCP server `{name}` (host config, or a trusted .mcp.json)");
    }
    println!("connecting to `{name}`…");
    let manager = crate::mcp::McpManager::new(cfg);
    match manager.list_tools(Some(name)).await {
        Ok(listing) => {
            println!("{listing}");
            Ok(())
        }
        Err(e) => bail!("could not reach `{name}`: {e}"),
    }
}

/// `cowboy mcp trust`: review + approve this repo's `.mcp.json` servers.
fn trust() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let servers = crate::mcp::trust::trust(&root)?;
    println!("✓ trusted {} server(s) from .mcp.json:", servers.len());
    for (name, s) in &servers {
        println!("  {name}  {}", s.transport_label());
    }
    println!("the agent can now use these via the `mcp` tool.");
    println!("re-run `cowboy mcp trust` if .mcp.json changes.");
    Ok(())
}

/// `cowboy mcp untrust`: revoke trust for this repo's `.mcp.json` servers.
fn untrust() -> Result<()> {
    let root = crate::cmd::project_root()?;
    if crate::mcp::trust::untrust(&root)? {
        println!("✓ revoked trust for this repo's .mcp.json servers");
    } else {
        println!("this repo's .mcp.json was not trusted");
    }
    Ok(())
}

/// Parse `KEY=VALUE` pairs into a map (used for `--env` and `--header`).
fn parse_kv(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("expected KEY=VALUE, got `{p}`"))?;
        if k.is_empty() {
            bail!("empty key in `{p}`");
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_splits_pairs() {
        let m = parse_kv(&["A=1".into(), "B=x=y".into()]).unwrap();
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "x=y"); // only the first `=` splits
        assert!(parse_kv(&["bad".into()]).is_err());
        assert!(parse_kv(&["=v".into()]).is_err());
    }
}
