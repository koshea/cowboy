//! Persisted network approvals (`.cowboy/approvals.json`).
//!
//! `project`/`global`-scoped approvals from the TUI are saved here and merged
//! into the network policy at session start, so the gateway allows them in
//! future sessions. We keep this separate from `security.yaml` so we never
//! rewrite the user's commented host config.

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

fn path(root: &Path) -> PathBuf {
    root.join(".cowboy").join("approvals.json")
}

/// Load persisted approvals (empty if none).
pub fn load(root: &Path) -> Vec<Approval> {
    std::fs::read_to_string(path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Append an approval derived from an attempt, de-duplicating.
pub fn append(root: &Path, attempt: &NetworkAttempt) -> std::io::Result<()> {
    let mut all = load(root);
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
        let dir = root.join(".cowboy");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(path(root), serde_json::to_string_pretty(&all)?)?;
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
        let tmp = assert_fs::TempDir::new().unwrap();
        let attempt = NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
        };
        append(tmp.path(), &attempt).unwrap();
        append(tmp.path(), &attempt).unwrap(); // dedup
        let all = load(tmp.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].host.as_deref(), Some("example.com"));
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
