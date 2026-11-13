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
///
/// Targets are under the agent's `HOME` (`/home/agent`), where the tools look.
/// On macOS, which cannot remap a path, they are linked into that `HOME` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    /// `(host source, in-sandbox target)` pairs.
    pub files: &'static [(&'static str, &'static str)],
    /// `(env name, host command whose stdout is the value)`.
    pub env_cmd: &'static [(&'static str, &'static str)],
    /// Host credential stores the preset deliberately does *not* grant, but
    /// which hold the same tool's secrets — so they are denylisted all the same,
    /// and an older grant of them is replaced when the preset is re-added.
    pub guards: &'static [&'static str],
    /// Suffix rules: `googleapis.com` also matches `storage.googleapis.com`.
    pub domains: &'static [&'static str],
    pub note: &'static str,
}

/// Preset names, in the order they are offered to the user.
pub const NAMES: &[&str] = &["gh", "gcloud", "gcloud-adc", "kubectl", "aws", "git", "ssh"];

pub fn preset(name: &str) -> Option<Preset> {
    Some(match name {
        "gh" => Preset {
            files: &[("~/.config/gh", "/home/agent/.config/gh")],
            // gh keeps the token in the OS keyring (not hosts.yml), so mounting
            // the config isn't enough — pull a fresh token from `gh auth token`.
            env_cmd: &[("GH_TOKEN", "gh auth token")],
            guards: &[],
            domains: &["api.github.com", "github.com"],
            note: "your GitHub CLI auth (config read-only + GH_TOKEN from the keyring).",
        },
        // Not the config directory: it holds the refresh tokens (`credentials.db`,
        // `legacy_credentials`), and gcloud refuses to run on a read-only one. The
        // agent gets a short-lived access token, refreshed host-side, and its own
        // writable gcloud config under its HOME.
        "gcloud" => Preset {
            files: &[],
            env_cmd: &[
                (
                    "CLOUDSDK_AUTH_ACCESS_TOKEN",
                    "gcloud auth print-access-token",
                ),
                ("CLOUDSDK_CORE_PROJECT", "gcloud config get-value project"),
            ],
            guards: &["~/.config/gcloud"],
            domains: &["accounts.google.com", "googleapis.com"],
            note: "a short-lived gcloud access token and your active project, refreshed \
                   from the host (no refresh token enters the sandbox). Client libraries \
                   use application-default credentials instead: see `gcloud-adc`.",
        },
        "gcloud-adc" => Preset {
            files: &[(
                "~/.config/gcloud/application_default_credentials.json",
                "/home/agent/.config/gcloud/application_default_credentials.json",
            )],
            env_cmd: &[],
            guards: &["~/.config/gcloud"],
            domains: &["accounts.google.com", "googleapis.com"],
            note: "your application-default credentials for Google client libraries \
                   (read-only). WARNING: the file holds a long-lived refresh token.",
        },
        "kubectl" => Preset {
            files: &[("~/.kube", "/home/agent/.kube")],
            env_cmd: &[],
            guards: &[],
            domains: &[],
            note: "your kubeconfig (read-only). Also allow your cluster's API server host.",
        },
        "aws" => Preset {
            files: &[("~/.aws", "/home/agent/.aws")],
            env_cmd: &[],
            guards: &[],
            domains: &["amazonaws.com"],
            note: "your AWS credentials/config (read-only).",
        },
        "git" => Preset {
            files: &[
                ("~/.gitconfig", "/home/agent/.gitconfig"),
                ("~/.git-credentials", "/home/agent/.git-credentials"),
            ],
            env_cmd: &[],
            guards: &[],
            domains: &["github.com"],
            note: "your git config + stored credentials (read-only).",
        },
        "ssh" => Preset {
            files: &[("~/.ssh", "/home/agent/.ssh")],
            env_cmd: &[],
            guards: &[],
            domains: &[],
            note: "WARNING: exposes your SSH PRIVATE KEYS to the agent (read-only).",
        },
        _ => return None,
    })
}

impl Preset {
    /// Every host path this preset grants or guards.
    pub fn sources(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.files
            .iter()
            .map(|(src, _)| *src)
            .chain(self.guards.iter().copied())
    }
}

/// Every host path any preset grants or guards, as written in the table (still
/// `~`-relative), minus those already inside another. The source of the sandbox
/// credential denylist, which matches a path and everything under it.
pub fn all_credential_sources() -> Vec<&'static str> {
    let all: Vec<&'static str> = NAMES
        .iter()
        .filter_map(|n| preset(n))
        .flat_map(|p| p.sources().collect::<Vec<_>>())
        .collect();
    let mut out: Vec<&'static str> = Vec::new();
    for src in &all {
        if !out.contains(src) && !all.iter().any(|o| is_under(src, o)) {
            out.push(src);
        }
    }
    out
}

/// Whether `path` is strictly inside `dir`.
fn is_under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
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
            for src in preset(n).unwrap().sources() {
                assert!(
                    srcs.iter().any(|s| *s == src || is_under(src, s)),
                    "{src} (preset {n}) missing from denylist"
                );
            }
        }
        // Spot-check the ones users are most likely to be asked for mid-task.
        for expect in [
            "~/.aws",
            "~/.ssh",
            "~/.kube",
            "~/.git-credentials",
            "~/.config/gcloud",
        ] {
            assert!(srcs.contains(&expect), "{expect} not covered");
        }
    }

    /// The agent's HOME moved from `/tmp` to `/home/agent`; a preset still aimed at
    /// `/tmp` mounts a credential where no tool looks.
    #[test]
    fn preset_targets_are_under_the_agent_home() {
        for n in NAMES {
            for (_, target) in preset(n).unwrap().files {
                assert!(target.starts_with("/home/agent/"), "{n}: {target}");
            }
        }
    }

    /// Domain rules are suffix matches; a literal `*.` never matches anything.
    #[test]
    fn preset_domains_are_plain_suffixes() {
        for n in NAMES {
            for d in preset(n).unwrap().domains {
                assert!(!d.contains('*'), "{n}: {d}");
            }
        }
    }
}
