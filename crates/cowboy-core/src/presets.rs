//! Known-tool credential presets.
//!
//! Lives in core because it has **two** consumers that must never disagree:
//! `cowboy secrets add <preset>`, which prints a paste-ready grant, and the
//! sandbox's credential denylist, which refuses to let the agent grant itself any
//! of these paths at runtime.
//!
//! That pairing is the point. `cowboy secrets add` is deliberately
//! non-destructive — it prints a grant the user adds to host-owned config
//! themselves — so a runtime approval modal must not become a softer route to the
//! same credentials. Deriving the denylist from this table means adding a preset
//! automatically extends the denylist, instead of the two drifting apart.

/// A known-tool preset: read-only file grants, env vars sourced from a host
/// command (for keyring-backed tokens), and the network it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    /// `(host source, in-sandbox target)` pairs.
    pub files: &'static [(&'static str, &'static str)],
    /// `(env name, host command whose stdout is the value)`.
    pub env_cmd: &'static [(&'static str, &'static str)],
    pub domains: &'static [&'static str],
    pub note: &'static str,
}

/// Preset names, in the order they are offered to the user.
pub const NAMES: &[&str] = &["gh", "gcloud", "kubectl", "aws", "git", "ssh"];

pub fn preset(name: &str) -> Option<Preset> {
    Some(match name {
        "gh" => Preset {
            files: &[("~/.config/gh", "/tmp/.config/gh")],
            // gh keeps the token in the OS keyring (not hosts.yml), so mounting
            // the config isn't enough — pull a fresh token from `gh auth token`.
            env_cmd: &[("GH_TOKEN", "gh auth token")],
            domains: &["api.github.com", "github.com"],
            note: "your GitHub CLI auth (config read-only + GH_TOKEN from the keyring).",
        },
        "gcloud" => Preset {
            files: &[("~/.config/gcloud", "/tmp/.config/gcloud")],
            env_cmd: &[],
            domains: &[
                "accounts.google.com",
                "oauth2.googleapis.com",
                "*.googleapis.com",
            ],
            note: "your gcloud config + application-default credentials (read-only). \
                   Token refresh needs write access — set read_only: false if it fails.",
        },
        "kubectl" => Preset {
            files: &[("~/.kube", "/tmp/.kube")],
            env_cmd: &[],
            domains: &[],
            note: "your kubeconfig (read-only). Also allow your cluster's API server host.",
        },
        "aws" => Preset {
            files: &[("~/.aws", "/tmp/.aws")],
            env_cmd: &[],
            domains: &["*.amazonaws.com"],
            note: "your AWS credentials/config (read-only).",
        },
        "git" => Preset {
            files: &[
                ("~/.gitconfig", "/tmp/.gitconfig"),
                ("~/.git-credentials", "/tmp/.git-credentials"),
            ],
            env_cmd: &[],
            domains: &["github.com"],
            note: "your git config + stored credentials (read-only).",
        },
        "ssh" => Preset {
            files: &[("~/.ssh", "/tmp/.ssh")],
            env_cmd: &[],
            domains: &[],
            note: "WARNING: exposes your SSH PRIVATE KEYS to the agent (read-only).",
        },
        _ => return None,
    })
}

/// Every host path any preset can grant, as written in the table (still
/// `~`-relative). The source of the sandbox credential denylist.
pub fn all_credential_sources() -> Vec<&'static str> {
    NAMES
        .iter()
        .filter_map(|n| preset(n))
        .flat_map(|p| p.files.iter().map(|(src, _)| *src).collect::<Vec<_>>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_resolves() {
        for n in NAMES {
            assert!(preset(n).is_some(), "preset {n} is listed but not defined");
        }
        assert!(preset("nope").is_none());
    }

    /// The denylist is only as good as this derivation: a preset whose files are
    /// missed here becomes a credential the agent could grant itself at runtime.
    #[test]
    fn credential_sources_cover_every_preset_file() {
        let srcs = all_credential_sources();
        for n in NAMES {
            for (src, _) in preset(n).unwrap().files {
                assert!(
                    srcs.contains(src),
                    "{src} (preset {n}) missing from denylist"
                );
            }
        }
        // Spot-check the ones users are most likely to be asked for mid-task.
        for expect in ["~/.aws", "~/.ssh", "~/.kube", "~/.git-credentials"] {
            assert!(srcs.contains(&expect), "{expect} not covered");
        }
    }
}
