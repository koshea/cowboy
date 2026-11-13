//! `cowboy mcp …` — manage the host-owned MCP server config
//! (`~/.config/cowboy/mcp.yaml`). MCP servers are trusted integrations the user
//! configures; the agent can call configured servers but never edit this file.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use cowboy_core::mcp::{self, McpConfig, McpServer, McpTransport};

use crate::cli::{McpAddArgs, McpCommand, Transport};

pub async fn run(command: McpCommand) -> Result<()> {
    match command {
        McpCommand::List => list(),
        McpCommand::Show { name } => show(&name),
        McpCommand::Add(args) => add(args),
        McpCommand::Remove { name } => remove(&name),
        McpCommand::Enable { name } => set_enabled(&name, true),
        McpCommand::Disable { name } => set_enabled(&name, false),
        McpCommand::Test { name } => test(&name).await,
        McpCommand::Login { name } => login(&name).await,
        McpCommand::Logout { name } => logout(&name),
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
            println!(
                "  {name}  [{state}]  {}{}{allow}",
                s.transport_label(),
                auth_note(s)
            );
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
                println!("  {name}  {}{}{note}", s.transport_label(), auth_note(s));
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
    // No catch-all arm: clap rejects anything that is not a `Transport`, so an unknown
    // value is reported against the flag with the valid values listed, before we get here.
    let transport = match args.transport {
        Transport::Stdio => {
            let command = args
                .command
                .context("`--command` is required for a stdio server")?;
            McpTransport::Stdio {
                command,
                args: args.args,
                env: parse_kv(&args.env).context("parsing --env")?,
            }
        }
        Transport::Http => {
            let url = args.url.context("`--url` is required for an http server")?;
            McpTransport::Http {
                url,
                headers: parse_kv(&args.header).context("parsing --header")?,
            }
        }
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
    crate::ui::ok(&format!("{verb} MCP server `{}`", args.name));
    println!("  check it with `cowboy mcp test {}`", args.name);
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let mut cfg = load()?;
    if cfg.servers.remove(name).is_none() {
        bail!("no MCP server `{name}`");
    }
    save(&cfg)?;
    crate::ui::ok(&format!("removed MCP server `{name}`"));
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
    crate::ui::ok(&format!(
        "{} MCP server `{name}`",
        if enabled { "enabled" } else { "disabled" }
    ));
    Ok(())
}

/// `  (signed in)` for a server that uses OAuth; nothing otherwise.
fn auth_note(s: &McpServer) -> String {
    let kind = crate::mcp::auth::AuthKind::of(s);
    if kind.uses_oauth() {
        format!("  ({})", kind.label())
    } else {
        String::new()
    }
}

/// Host servers plus this repo's trusted `.mcp.json` ones (host config wins).
fn effective() -> Result<McpConfig> {
    let mut cfg = load()?;
    if let Ok(root) = crate::cmd::project_root() {
        for (n, s) in crate::mcp::trust::trusted_servers(&root) {
            cfg.servers.entry(n).or_insert(s);
        }
    }
    Ok(cfg)
}

/// The URL of a server that signs in with OAuth.
fn oauth_url(cfg: &McpConfig, name: &str) -> Result<String> {
    let s = cfg
        .servers
        .get(name)
        .with_context(|| format!("no MCP server `{name}` (host config, or a trusted .mcp.json)"))?;
    match (&s.transport, crate::mcp::auth::AuthKind::of(s)) {
        (_, crate::mcp::auth::AuthKind::Local) => {
            bail!("`{name}` is a local process; it handles its own authentication")
        }
        (_, crate::mcp::auth::AuthKind::Header) => {
            bail!("`{name}` authenticates with the Authorization header in its config")
        }
        (McpTransport::Http { url, .. }, _) => Ok(url.clone()),
        (McpTransport::Stdio { .. }, _) => unreachable!("a stdio server is Local"),
    }
}

async fn login(name: &str) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let cfg = effective()?;
    let url = oauth_url(&cfg, name)?;
    let mut login = crate::mcp::auth::Login::begin(&url).await?;
    let link = login.url().to_string();
    println!("Sign in to `{name}` in your browser:\n\n  {link}\n");
    if crate::mcp::auth::open_browser(&link) {
        println!("(asked your browser to open it)");
    }
    println!(
        "Waiting for the browser (up to 5 minutes). If it is on another machine, paste \
         the address its last page tried to load and press Enter."
    );
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let pasted = async {
        loop {
            match stdin.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => return Some(line),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    };
    let callback = tokio::select! {
        r = tokio::time::timeout(crate::mcp::auth::LOGIN_TIMEOUT, login.callback()) => {
            r.context("timed out waiting for the browser; run the login again")??
        }
        // A closed stdin (a script) is not an answer; keep waiting on the browser.
        Some(line) = pasted => line,
    };
    login.finish(&callback).await?;
    crate::ui::ok(&format!("signed in to `{name}`"));
    let manager = crate::mcp::McpManager::new(cfg);
    match manager.exposed_tools(name).await {
        Ok((exposed, total)) => println!(
            "  connected: {} of its {total} tools exposed by the allowlist",
            exposed.len()
        ),
        Err(e) => crate::ui::warn(&format!("signed in, but connecting failed: {e}")),
    }
    Ok(())
}

fn logout(name: &str) -> Result<()> {
    let url = oauth_url(&effective()?, name)?;
    if crate::mcp::auth::sign_out(&url)? {
        crate::ui::ok(&format!("signed out of `{name}` on this machine"));
        println!("  (revoke Cowboy's access in the service's settings to end the grant itself)");
    } else {
        println!("`{name}` was not signed in");
    }
    Ok(())
}

async fn test(name: &str) -> Result<()> {
    let cfg = effective()?;
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
///
/// A gate, not a receipt. This used to trust first and print the server list after,
/// which is exactly backwards for the one command whose whole job is consent: a stdio
/// server is an arbitrary host command that arrived with a clone.
fn trust() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let pending = crate::mcp::trust::pending(&root)?;
    if pending.is_empty() {
        crate::ui::info("this repo's .mcp.json declares no servers; nothing to trust");
        return Ok(());
    }

    crate::ui::heading(&format!(
        "{} server(s) declared by {}",
        pending.len(),
        root.join(".mcp.json").display()
    ));
    for (name, s) in &pending {
        println!("\n  {}", crate::style::bold(name));
        crate::ui::kv("purpose", &s.description);
        match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                // The full argv, not just the binary: `npx -y some-package` is the part
                // that decides what actually runs.
                let argv = std::iter::once(command.as_str())
                    .chain(args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ");
                crate::ui::kv("runs (stdio)", &argv);
                if !env.is_empty() {
                    // Names only — a value here may be a `${VAR}` reference the host
                    // expands, and printing expansions would leak the secret.
                    let names = env.keys().cloned().collect::<Vec<_>>().join(", ");
                    crate::ui::kv("env", &names);
                }
            }
            McpTransport::Http { url, headers } => {
                crate::ui::kv("connects to", url);
                if !headers.is_empty() {
                    let names = headers.keys().cloned().collect::<Vec<_>>().join(", ");
                    crate::ui::kv("headers", &names);
                }
            }
        }
        crate::ui::kv("tools", &s.tools.join(", "));
    }

    crate::ui::warn("\ntrusting these lets the agent run them for this project.");
    if !crate::prompt::confirm_destructive("Trust them?")? {
        return Ok(());
    }

    let servers = crate::mcp::trust::trust(&root)?;
    crate::ui::ok(&format!(
        "trusted {} server(s) from .mcp.json",
        servers.len()
    ));
    crate::ui::step("the agent can now use these via the `mcp` tool");
    crate::ui::step("re-run `cowboy mcp trust` if .mcp.json changes");
    Ok(())
}

/// `cowboy mcp untrust`: revoke trust for this repo's `.mcp.json` servers.
fn untrust() -> Result<()> {
    let root = crate::cmd::project_root()?;
    if crate::mcp::trust::untrust(&root)? {
        crate::ui::ok("revoked trust for this repo's .mcp.json servers");
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
