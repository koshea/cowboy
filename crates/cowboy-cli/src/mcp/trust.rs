//! Trust gate for a repo's project MCP servers (`.mcp.json`).
//!
//! A repo's `.mcp.json` can declare MCP servers — but a stdio server runs an
//! arbitrary host command, so those servers are **inert until the user explicitly
//! trusts them** with `cowboy mcp trust`. Trust is recorded **host-side**, keyed by
//! project, and pinned to the approved server set: any later change to `.mcp.json`
//! flips the project to `Stale` and requires re-trusting.
//!
//! SECURITY: like `net/approvals.rs`, the trust record lives under
//! `~/.config/cowboy/mcp-trust/`, NEVER in the (agent-writable) workspace — the
//! agent must not be able to trust servers on its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use cowboy_core::mcp::{load_project_mcp, McpServer};
use cowboy_core::time::now_ms;
use serde::{Deserialize, Serialize};

/// The trust state of a repo's `.mcp.json` relative to what the user approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// No `.mcp.json` in the repo.
    NoFile,
    /// A `.mcp.json` exists but the user has not trusted it (or it doesn't parse).
    Untrusted,
    /// Trusted previously, but the file changed since — needs re-trusting.
    Stale,
    /// The current `.mcp.json` matches what the user approved.
    Trusted,
}

impl TrustState {
    pub fn label(self) -> &'static str {
        match self {
            TrustState::NoFile => "none",
            TrustState::Untrusted => "untrusted",
            TrustState::Stale => "stale — re-run `cowboy mcp trust`",
            TrustState::Trusted => "trusted",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TrustRecord {
    /// The exact server set the user approved (semantic compare against current).
    servers: BTreeMap<String, McpServer>,
    approved_ms: u64,
}

/// Host-only trust dir (`~/.config/cowboy/mcp-trust/`); falls back to the host temp
/// dir if there's no home config dir. Never inside the workspace.
fn trust_dir() -> PathBuf {
    cowboy_core::config::global_config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mcp-trust")
}

/// Trust record file for a project root within `dir` (keyed by the root's hash).
fn file_in(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!(
        "{:08x}.json",
        crate::net::runtime::project_hash(root)
    ))
}

fn load_record_in(dir: &Path, root: &Path) -> Option<TrustRecord> {
    std::fs::read_to_string(file_in(dir, root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// The current trust state of `root`'s `.mcp.json`.
pub fn project_trust(root: &Path) -> TrustState {
    trust_state_in(&trust_dir(), root)
}

fn trust_state_in(dir: &Path, root: &Path) -> TrustState {
    let current = match load_project_mcp(root) {
        Ok(Some(s)) => s,
        Ok(None) => return TrustState::NoFile,
        // Present but unparseable → treat as untrusted (CLI re-parse surfaces why).
        Err(_) => return TrustState::Untrusted,
    };
    match load_record_in(dir, root) {
        Some(rec) if rec.servers == current => TrustState::Trusted,
        Some(_) => TrustState::Stale,
        None => TrustState::Untrusted,
    }
}

/// The project servers iff the repo's `.mcp.json` is currently trusted, else empty.
pub fn trusted_servers(root: &Path) -> BTreeMap<String, McpServer> {
    if project_trust(root) == TrustState::Trusted {
        load_project_mcp(root).ok().flatten().unwrap_or_default()
    } else {
        BTreeMap::new()
    }
}

/// Approve the repo's current `.mcp.json` server set. Returns the approved servers
/// (for display). Errors if there is no `.mcp.json` or it doesn't parse.
pub fn trust(root: &Path) -> Result<BTreeMap<String, McpServer>> {
    trust_in(&trust_dir(), root)
}

fn trust_in(dir: &Path, root: &Path) -> Result<BTreeMap<String, McpServer>> {
    let servers = load_project_mcp(root)
        .context("reading .mcp.json")?
        .ok_or_else(|| anyhow!("no .mcp.json in this repo"))?;
    let rec = TrustRecord {
        servers: servers.clone(),
        approved_ms: now_ms(),
    };
    std::fs::create_dir_all(dir).context("creating mcp-trust dir")?;
    let json = serde_json::to_string_pretty(&rec)?;
    std::fs::write(file_in(dir, root), json).context("writing trust record")?;
    Ok(servers)
}

/// Revoke trust for `root`. Returns true if a record existed.
pub fn untrust(root: &Path) -> Result<bool> {
    untrust_in(&trust_dir(), root)
}

fn untrust_in(dir: &Path, root: &Path) -> Result<bool> {
    let f = file_in(dir, root);
    if f.exists() {
        std::fs::remove_file(&f).context("removing trust record")?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_mcp_json(root: &Path, body: &str) {
        std::fs::write(root.join(".mcp.json"), body).unwrap();
    }

    #[test]
    fn trust_lifecycle_nofile_untrusted_trusted_stale() {
        let cfg = assert_fs::TempDir::new().unwrap();
        let repo = assert_fs::TempDir::new().unwrap();
        let dir = cfg.path();
        let root = repo.path();

        // No file yet.
        assert_eq!(trust_state_in(dir, root), TrustState::NoFile);

        // File present, not trusted.
        write_mcp_json(
            root,
            r#"{"mcpServers":{"fs":{"command":"server-fs","args":["/w"]}}}"#,
        );
        assert_eq!(trust_state_in(dir, root), TrustState::Untrusted);

        // Trust it → trusted, and the servers are now returned.
        let approved = trust_in(dir, root).unwrap();
        assert!(approved.contains_key("fs"));
        assert_eq!(trust_state_in(dir, root), TrustState::Trusted);

        // Change the file → stale (a new server appears).
        write_mcp_json(
            root,
            r#"{"mcpServers":{"fs":{"command":"server-fs","args":["/w"]},
                              "git":{"command":"server-git"}}}"#,
        );
        assert_eq!(trust_state_in(dir, root), TrustState::Stale);

        // Re-trust picks up the new set.
        trust_in(dir, root).unwrap();
        assert_eq!(trust_state_in(dir, root), TrustState::Trusted);

        // Untrust → back to untrusted; the record file is gone.
        assert!(untrust_in(dir, root).unwrap());
        assert_eq!(trust_state_in(dir, root), TrustState::Untrusted);
        assert!(!untrust_in(dir, root).unwrap());

        // The trust record lives under the config dir, not the workspace.
        assert!(!root.join(".cowboy").exists());
    }

    #[test]
    fn trust_errors_without_a_file() {
        let cfg = assert_fs::TempDir::new().unwrap();
        let repo = assert_fs::TempDir::new().unwrap();
        assert!(trust_in(cfg.path(), repo.path()).is_err());
        assert!(super::trusted_servers(repo.path()).is_empty());
    }
}
