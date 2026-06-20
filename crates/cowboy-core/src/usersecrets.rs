//! Personal (host-side) credential-grant overlay, merged with a project's
//! `.cowboy/security.yaml`.
//!
//! Some credential grants are personal preference and per-machine paths that
//! shouldn't be committed into a repo's opinionated `security.yaml`. Those live
//! in the home dir: a cross-project `global.yaml` and a per-**repository**
//! `projects/<key>.yaml` (key derived from the main repo, so a grant applies to
//! every worktree of that repo). At session start the effective config is:
//!
//! ```text
//! repo .cowboy/security.yaml  ∪  ~/.config/cowboy/secrets/global.yaml
//!                            ∪  ~/.config/cowboy/secrets/projects/<repo-key>.yaml
//! ```
//!
//! Only credential grants (`env`, `files`) and the additive network `allow`-list
//! merge from the user overlay; the project still owns the rest of the security
//! boundary (deny, default verdicts, container, isolation). The overlay is
//! host-owned (home dir, never mounted) so the agent can't grant itself anything.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{global_config_dir, RuleSet, SecretEnv, SecretMount, SecurityConfig};
use crate::error::{Error, Result};

/// One user-overlay file's grants.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserSecrets {
    #[serde(default)]
    pub env: Vec<SecretEnv>,
    #[serde(default)]
    pub files: Vec<SecretMount>,
    /// Additive network allow-list (domains/cidrs/ports) these grants need.
    #[serde(default)]
    pub allow: RuleSet,
}

/// Where the overlay lives: `~/.config/cowboy/secrets`.
fn base() -> Option<PathBuf> {
    global_config_dir().map(|d| d.join("secrets"))
}

/// The cross-project overlay file (`secrets/global.yaml`).
pub fn global_file() -> Option<PathBuf> {
    base().map(|b| b.join("global.yaml"))
}

/// The per-repository overlay file (`secrets/projects/<key>.yaml`), shared by
/// all of the repo's worktrees.
pub fn project_file(key: &str) -> Option<PathBuf> {
    base().map(|b| b.join("projects").join(format!("{key}.yaml")))
}

/// Read an overlay file (an absent/unreadable file is an empty overlay).
pub fn read(path: &Path) -> UserSecrets {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_yaml_ng::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write an overlay file (creates the dir 0700, atomic temp+rename).
pub fn write(path: &Path, secrets: &UserSecrets) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent);
    }
    let yaml = serde_yaml_ng::to_string(secrets).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Merge the global + per-project user overlay into a project's security config
/// (additive union, deduped). Run sites call this after loading security.yaml,
/// then re-`validate()`.
pub fn merge_into(security: &mut SecurityConfig, project_key: &str) {
    if let Some(b) = base() {
        merge_into_base(security, &b, project_key);
    }
}

/// Testable core: merge overlays found under `base`.
pub fn merge_into_base(security: &mut SecurityConfig, base: &Path, project_key: &str) {
    let global = read(&base.join("global.yaml"));
    let project = read(&base.join("projects").join(format!("{project_key}.yaml")));
    for overlay in [global, project] {
        apply(security, overlay);
    }
}

fn apply(security: &mut SecurityConfig, overlay: UserSecrets) {
    for e in overlay.env {
        if !security.secrets.env.iter().any(|x| x.name == e.name) {
            security.secrets.env.push(e);
        }
    }
    for f in overlay.files {
        if !security
            .secrets
            .files
            .iter()
            .any(|x| x.source == f.source && x.target == f.target)
        {
            security.secrets.files.push(f);
        }
    }
    let allow = &mut security.network_policy.allow;
    merge_unique(&mut allow.domains, overlay.allow.domains);
    merge_unique(&mut allow.cidrs, overlay.allow.cidrs);
    for p in overlay.allow.ports {
        if !allow.ports.contains(&p) {
            allow.ports.push(p);
        }
    }
}

fn merge_unique(into: &mut Vec<String>, from: Vec<String>) {
    for s in from {
        if !into.contains(&s) {
            into.push(s);
        }
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cowboy-usec-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn merges_global_and_project_with_repo() {
        let base = tmp();
        write(
            &base.join("global.yaml"),
            &UserSecrets {
                files: vec![SecretMount {
                    source: "~/.gitconfig".into(),
                    target: "/tmp/.gitconfig".into(),
                    read_only: true,
                    required: false,
                    approval: None,
                }],
                allow: RuleSet {
                    domains: vec!["github.com".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        write(
            &base.join("projects").join("abcd1234.yaml"),
            &UserSecrets {
                files: vec![SecretMount {
                    source: "~/.config/gh".into(),
                    target: "/tmp/.config/gh".into(),
                    read_only: true,
                    required: false,
                    approval: None,
                }],
                allow: RuleSet {
                    domains: vec!["api.github.com".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        let mut sec = SecurityConfig::default();
        sec.network_policy.allow.domains.clear();
        merge_into_base(&mut sec, &base, "abcd1234");

        let targets: Vec<_> = sec
            .secrets
            .files
            .iter()
            .map(|f| f.target.as_str())
            .collect();
        assert!(targets.contains(&"/tmp/.gitconfig")); // from global
        assert!(targets.contains(&"/tmp/.config/gh")); // from this project
        assert!(sec
            .network_policy
            .allow
            .domains
            .contains(&"github.com".to_string()));
        assert!(sec
            .network_policy
            .allow
            .domains
            .contains(&"api.github.com".to_string()));

        // A different project key does NOT pick up abcd1234's per-project grant.
        let mut other = SecurityConfig::default();
        merge_into_base(&mut other, &base, "99999999");
        assert!(!other
            .secrets
            .files
            .iter()
            .any(|f| f.target == "/tmp/.config/gh"));
        assert!(other
            .secrets
            .files
            .iter()
            .any(|f| f.target == "/tmp/.gitconfig"));
    }

    #[test]
    fn merged_footgun_grant_is_caught_by_validate() {
        // The overlay is host-owned, but a foot-gun grant in it (a credential
        // whose source is a host secret) must still be rejected once merged — the
        // merge+validate contract is what callers rely on. This locks it in.
        let base = tmp();
        write(
            &base.join("global.yaml"),
            &UserSecrets {
                files: vec![SecretMount {
                    source: "~/.config/cowboy/providers.yaml".into(),
                    target: "/tmp/keys".into(),
                    read_only: true,
                    required: false,
                    approval: None,
                }],
                ..Default::default()
            },
        )
        .unwrap();

        let mut sec = SecurityConfig::default();
        merge_into_base(&mut sec, &base, "abcd1234");
        assert!(
            matches!(sec.validate(), Err(crate::Error::SecurityInvariant(_))),
            "a merged grant exposing providers.yaml must fail validate()"
        );
    }

    #[test]
    fn merge_is_deduped() {
        let base = tmp();
        let grant = UserSecrets {
            files: vec![SecretMount {
                source: "~/.config/gh".into(),
                target: "/tmp/.config/gh".into(),
                read_only: true,
                required: false,
                approval: None,
            }],
            allow: RuleSet {
                domains: vec!["api.github.com".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        write(&base.join("global.yaml"), &grant).unwrap();
        let mut sec = SecurityConfig::default();
        // Repo already has the same grant + domain; overlay must not duplicate.
        sec.secrets.files.push(grant.files[0].clone());
        sec.network_policy.allow.domains = vec!["api.github.com".into()];
        merge_into_base(&mut sec, &base, "k");
        assert_eq!(
            sec.secrets
                .files
                .iter()
                .filter(|f| f.target == "/tmp/.config/gh")
                .count(),
            1
        );
        assert_eq!(
            sec.network_policy
                .allow
                .domains
                .iter()
                .filter(|d| *d == "api.github.com")
                .count(),
            1
        );
    }

    #[test]
    fn empty_overlay_is_a_noop() {
        let base = tmp();
        let mut sec = SecurityConfig::default();
        let before = sec.secrets.files.len();
        merge_into_base(&mut sec, &base, "none");
        assert_eq!(sec.secrets.files.len(), before);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let base = tmp();
        let path = base.join("global.yaml");
        let us = UserSecrets {
            env: vec![SecretEnv {
                name: "GH_TOKEN".into(),
                source_env: "GH_TOKEN".into(),
                source_command: None,
                required: false,
                approval: None,
            }],
            ..Default::default()
        };
        write(&path, &us).unwrap();
        let back = read(&path);
        assert_eq!(back.env.len(), 1);
        assert_eq!(back.env[0].name, "GH_TOKEN");
    }
}
