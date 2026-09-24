//! External agent harnesses — vendor coding-agent CLIs (grok, …) the crew can
//! delegate to, so work runs on the user's *subscription* plans, which only work
//! inside the vendor's own harness.
//!
//! Configured in the **user-level** `~/.config/cowboy/harnesses.yaml` only. A
//! definition decides what a sandboxed process may see (the vendor's login) and
//! reach (its API hosts), so it is host-owned like `providers.yaml`: there is no
//! project-level file a cloned repo could ship. `crew.yaml` only *routes* to these
//! names; a name defined here and in `models.yaml` is a configuration error.
//!
//! A harness always runs **inside** cowboy's sandbox with its own approval gates
//! and sandbox turned off — safe only because cowboy's kernel boundary confines it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::global_config_dir;
use crate::error::{Error, Result};

/// The file name under the user config dir.
pub const HARNESSES_FILE: &str = "harnesses.yaml";

/// `harnesses.yaml`: one entry per CLI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessesConfig {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessDef>,
}

fn one() -> u32 {
    1
}

/// One configured harness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDef {
    /// Which CLI this is; decides the command line, stream parser and defaults.
    pub kind: HarnessKind,
    /// Model to run (the CLI's own model id). `None` = the CLI's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The binary, when it is not the kind's default name on `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,
    /// How much of the vendor's login state the sandboxed harness gets.
    #[serde(default)]
    pub auth: AuthExposure,
    /// Extra command-line arguments, appended after cowboy's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Hosts to allow on top of the kind's built-in API/login hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,
    /// Minutes without output before the foreman is told the job looks stalled.
    /// Nothing is ever killed for it.
    #[serde(default = "default_stall_minutes")]
    pub stall_minutes: u32,
    /// Extra environment for the harness process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

fn default_stall_minutes() -> u32 {
    10
}

/// The CLIs cowboy knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Grok,
}

/// How the vendor login reaches the sandboxed harness. Whatever it gets is
/// readable by that harness's own model — treat it as disclosed to it.
///
/// Either way the harness works on a **private copy** in a job-scoped home, and
/// only a refreshed login file is written back. The user's real vendor home is
/// never mounted: it holds the vendor binary, hooks and MCP server commands that
/// the *host* later runs unconfined, so a writable mount would let the harness's
/// model plant code outside the sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthExposure {
    /// Only the login file. Nothing else of the vendor's home (other credentials,
    /// session history, config) is visible.
    #[default]
    AuthFile,
    /// The vendor home's configuration and credentials too — config, MCP
    /// credentials, skills, plugins — so the harness behaves as it does for you.
    /// Binaries, logs and session history are left out.
    FullHome,
}

/// What cowboy knows about a kind of CLI.
#[derive(Debug, Clone, Copy)]
pub struct KindSpec {
    /// Default binary name, resolved on the host's `PATH`.
    pub binary: &'static str,
    /// The vendor's home, `~`-relative.
    pub home: &'static str,
    /// The login file, relative to the home.
    pub auth_file: &'static str,
    /// The env var that relocates the vendor home.
    pub home_env: &'static str,
    /// Entries of the vendor home a `full_home` copy leaves out: binaries, caches,
    /// logs, session history, sockets and lock files.
    pub home_skip: &'static [&'static str],
    /// API and login hosts the harness cannot work without.
    pub hosts: &'static [&'static str],
    /// Environment that turns off telemetry, auto-update and anything interactive.
    pub env: &'static [(&'static str, &'static str)],
}

impl HarnessKind {
    pub fn spec(self) -> KindSpec {
        match self {
            HarnessKind::Grok => KindSpec {
                binary: "grok",
                home: "~/.grok",
                auth_file: "auth.json",
                home_env: "GROK_HOME",
                home_skip: &[
                    "bin",
                    "downloads",
                    "bundled",
                    "vendor",
                    "sessions",
                    "logs",
                    "debug",
                    "grove",
                    "memtrace",
                    "tmp",
                    "marketplace-cache",
                    "long-running-background-tasks",
                    "worktrees.db",
                    "leader.sock",
                ],
                hosts: &[
                    "cli-chat-proxy.grok.com",
                    "code.grok.com",
                    "api.x.ai",
                    "auth.x.ai",
                    "accounts.x.ai",
                ],
                env: &[
                    ("GROK_TELEMETRY_ENABLED", "0"),
                    ("DISABLE_TELEMETRY", "1"),
                    ("GROK_AUTO_UPDATE", "0"),
                    ("GROK_AGENT_DASHBOARD", "0"),
                ],
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::Grok => "grok",
        }
    }
}

impl HarnessesConfig {
    /// `~/.config/cowboy/harnesses.yaml`.
    pub fn user_path() -> Option<PathBuf> {
        global_config_dir().map(|d| d.join(HARNESSES_FILE))
    }

    /// Parse a harnesses file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    /// Parse `text` (read from `path`, which is only used in errors).
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let cfg: Self = serde_yaml_ng::from_str(text).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// The user's harnesses, or an empty set when there is no file.
    pub fn load_user() -> Result<Self> {
        match Self::user_path() {
            Some(p) if p.exists() => Self::load(&p),
            _ => Ok(Self::default()),
        }
    }

    pub fn get(&self, name: &str) -> Option<&HarnessDef> {
        self.harnesses.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.harnesses.keys().map(String::as_str)
    }

    fn validate(&self) -> Result<()> {
        for (name, def) in &self.harnesses {
            if name.trim().is_empty() || name.contains(char::is_whitespace) {
                return Err(Error::Invalid(format!(
                    "harnesses.yaml: harness name {name:?} must be a single word"
                )));
            }
            for k in def.env.keys() {
                if k.is_empty() || k.contains('=') {
                    return Err(Error::Invalid(format!(
                        "harnesses.yaml: `{name}` has an invalid env name {k:?}"
                    )));
                }
            }
        }
        Ok(())
    }
}

impl HarnessDef {
    /// The API/login hosts this harness is allowed to reach without asking.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .kind
            .spec()
            .hosts
            .iter()
            .map(|h| h.to_string())
            .collect();
        hosts.extend(self.allow_hosts.iter().cloned());
        hosts.sort();
        hosts.dedup();
        hosts
    }
}

/// Names defined both as a model and as a harness — a crew slot naming one would
/// be ambiguous, so it is refused rather than resolved by a precedence rule.
pub fn colliding_names<'a>(
    models: impl IntoIterator<Item = &'a str>,
    harnesses: &'a HarnessesConfig,
) -> Vec<String> {
    let models: std::collections::BTreeSet<&str> = models.into_iter().collect();
    harnesses
        .names()
        .filter(|h| models.contains(h))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<HarnessesConfig> {
        HarnessesConfig::parse(yaml, Path::new(HARNESSES_FILE))
    }

    #[test]
    fn a_minimal_entry_gets_safe_defaults() {
        let cfg = parse("harnesses:\n  grok:\n    kind: grok\n").unwrap();
        let g = cfg.get("grok").unwrap();
        assert_eq!(g.kind, HarnessKind::Grok);
        assert_eq!(
            g.auth,
            AuthExposure::AuthFile,
            "only the login file by default"
        );
        assert_eq!(g.stall_minutes, 10);
        assert!(g.hosts().iter().any(|h| h == "cli-chat-proxy.grok.com"));
    }

    #[test]
    fn unknown_fields_and_kinds_are_errors() {
        assert!(parse("harnesses:\n  grok:\n    kind: grok\n    api_key: x\n").is_err());
        assert!(parse("harnesses:\n  x:\n    kind: vim\n").is_err());
        assert!(parse("harnesses:\n  \"two words\":\n    kind: grok\n").is_err());
    }

    #[test]
    fn full_home_exposure_is_opt_in_and_hosts_merge() {
        let cfg = parse(
            "harnesses:\n  grok:\n    kind: grok\n    model: grok-4.7\n    auth: full_home\n    allow_hosts: [example.com, api.x.ai]\n",
        )
        .unwrap();
        let g = cfg.get("grok").unwrap();
        assert_eq!(g.auth, AuthExposure::FullHome);
        let hosts = g.hosts();
        assert!(hosts.contains(&"example.com".to_string()));
        assert_eq!(hosts.iter().filter(|h| *h == "api.x.ai").count(), 1);
    }

    #[test]
    fn a_name_defined_as_both_model_and_harness_is_reported() {
        let cfg = parse("harnesses:\n  grok:\n    kind: grok\n").unwrap();
        assert_eq!(colliding_names(["grok", "sonnet"], &cfg), vec!["grok"]);
        assert!(colliding_names(["sonnet"], &cfg).is_empty());
    }
}
