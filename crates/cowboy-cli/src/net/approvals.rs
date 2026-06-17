//! Persisted network approvals.
//!
//! `project`/`global`-scoped approvals from the TUI are saved here and merged
//! into the network policy at session start, so the gateway allows them in
//! future sessions. We keep this separate from `security.yaml` so we never
//! rewrite the user's commented host config.
//!
//! SECURITY: these are stored **host-side**, under `~/.config/cowboy/approvals/`
//! (keyed by project), NOT inside the project workspace. The workspace is
//! bind-mounted read-write into the agent container, so an approvals file there
//! would let a malicious model/repo widen its own network allow-list by writing
//! the file — the agent must never be able to grant itself egress.

use std::path::{Path, PathBuf};

use cowboy_core::config::NetworkPolicy;
use cowboy_core::netproto::NetworkAttempt;
use serde::{Deserialize, Serialize};

/// One persisted allow entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    pub port: u16,
}

/// The host-only directory holding per-project approvals
/// (`~/.config/cowboy/approvals/`); falls back to the host temp dir if there is
/// no home config dir. Never inside the (agent-writable) workspace.
fn approvals_dir() -> PathBuf {
    cowboy_core::config::global_config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("approvals")
}

/// Approvals file for a project root within `dir` (keyed by the root's hash).
fn file_in(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!("{:08x}.json", super::runtime::project_hash(root)))
}

/// Load persisted approvals (empty if none).
pub fn load(root: &Path) -> Vec<Approval> {
    load_in(&approvals_dir(), root)
}

/// Append an approval derived from an attempt, de-duplicating.
pub fn append(root: &Path, attempt: &NetworkAttempt) -> std::io::Result<()> {
    append_in(&approvals_dir(), root, attempt)
}

fn load_in(dir: &Path, root: &Path) -> Vec<Approval> {
    std::fs::read_to_string(file_in(dir, root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn append_in(dir: &Path, root: &Path, attempt: &NetworkAttempt) -> std::io::Result<()> {
    let mut all = load_in(dir, root);
    let entry = Approval {
        host: attempt.host.clone(),
        cidr: attempt
            .host
            .is_none()
            .then(|| attempt.ip.map(|ip| format!("{ip}/32")))
            .flatten(),
        port: attempt.port,
    };
    if !all.contains(&entry) {
        all.push(entry);
        std::fs::create_dir_all(dir)?;
        std::fs::write(file_in(dir, root), serde_json::to_string_pretty(&all)?)?;
    }
    Ok(())
}

/// Merge persisted approvals into a policy's allow-list (used when generating
/// the gateway policy file).
pub fn merge_into(policy: &mut NetworkPolicy, approvals: &[Approval]) {
    for a in approvals {
        if let Some(h) = &a.host {
            if !policy.allow.domains.contains(h) {
                policy.allow.domains.push(h.clone());
            }
        }
        if let Some(c) = &a.cidr {
            if !policy.allow.cidrs.contains(c) {
                policy.allow.cidrs.push(c.clone());
            }
        }
        if !policy.allow.ports.is_empty() && !policy.allow.ports.contains(&a.port) {
            policy.allow.ports.push(a.port);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::netproto::Protocol;

    #[test]
    fn append_and_load_roundtrip() {
        // Use an explicit (temp) approvals dir so the test never touches the real
        // host config dir — mirrors the host-only storage location.
        let cfg = assert_fs::TempDir::new().unwrap();
        let proj = assert_fs::TempDir::new().unwrap();
        let attempt = NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
        };
        append_in(cfg.path(), proj.path(), &attempt).unwrap();
        append_in(cfg.path(), proj.path(), &attempt).unwrap(); // dedup
        let all = load_in(cfg.path(), proj.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].host.as_deref(), Some("example.com"));
        // The approvals file is under the config dir, not the project workspace.
        assert!(!proj.path().join(".cowboy/approvals.json").exists());
    }

    #[test]
    fn merge_adds_domains_and_cidrs() {
        let mut policy = NetworkPolicy::default();
        policy.allow.ports = vec![443];
        merge_into(
            &mut policy,
            &[
                Approval {
                    host: Some("a.test".into()),
                    cidr: None,
                    port: 443,
                },
                Approval {
                    host: None,
                    cidr: Some("9.9.9.9/32".into()),
                    port: 53,
                },
            ],
        );
        assert!(policy.allow.domains.contains(&"a.test".to_string()));
        assert!(policy.allow.cidrs.contains(&"9.9.9.9/32".to_string()));
        assert!(policy.allow.ports.contains(&53));
    }
}
