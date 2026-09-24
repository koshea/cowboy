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
    /// xAI's Grok Build (`grok`).
    Grok,
    /// Anthropic's Claude Code (`claude`).
    Claude,
    /// OpenAI's Codex CLI (`codex`).
    Codex,
    /// Google's Antigravity CLI (`agy`).
    Agy,
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
///
/// Every harness job gets a private home directory as its `HOME`; the vendor's
/// files are copied there at the same `~`-relative paths, so each CLI finds its
/// login where it always looks and no per-vendor relocation variable is needed.
#[derive(Debug, Clone, Copy)]
pub struct KindSpec {
    /// Default binary name, resolved on the host's `PATH`.
    pub binary: &'static str,
    /// How much of the install the sandbox needs: `0` = the binary file alone,
    /// `n` = the directory `n` levels above it (a CLI that runs sibling helpers).
    pub install_levels: usize,
    /// Login files, `~`-relative. The first is the credential itself: its absence
    /// means "not logged in", and a refresh of it is written back to the host. The
    /// rest are account state, copied in when present and never written back.
    pub auth_files: &'static [&'static str],
    /// Top-level keys stripped from a JSON account-state file for `auth_file`
    /// exposure — configuration that is not the login (Claude Code keeps the user's
    /// MCP server definitions in `~/.claude.json`, which would start them).
    pub strip_keys: &'static [&'static str],
    /// The vendor's config directories, `~`-relative, copied for `full_home`.
    pub vendor_dirs: &'static [&'static str],
    /// Entries of a vendor dir a `full_home` copy leaves out: binaries, caches,
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
                install_levels: 0,
                auth_files: &[".grok/auth.json"],
                strip_keys: &[],
                vendor_dirs: &[".grok"],
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
            HarnessKind::Claude => KindSpec {
                binary: "claude",
                install_levels: 0,
                auth_files: &[".claude/.credentials.json", ".claude.json"],
                strip_keys: &["mcpServers", "projects"],
                vendor_dirs: &[".claude"],
                home_skip: &[
                    "projects",
                    "sessions",
                    "shell-snapshots",
                    "file-history",
                    "todos",
                    "statsig",
                    "logs",
                    "debug",
                    "cache",
                    "ide",
                    "local",
                    "downloads",
                ],
                hosts: &[
                    "api.anthropic.com",
                    "claude.ai",
                    "platform.claude.com",
                    "console.anthropic.com",
                ],
                env: &[
                    // The sandbox runs commands as uid 0 in its user namespace, and
                    // Claude Code refuses to skip its permission prompts as root unless
                    // told it is sandboxed — which it is.
                    ("IS_SANDBOX", "1"),
                    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
                    ("DISABLE_TELEMETRY", "1"),
                    ("DISABLE_ERROR_REPORTING", "1"),
                    ("DISABLE_AUTOUPDATER", "1"),
                ],
            },
            HarnessKind::Codex => KindSpec {
                binary: "codex",
                // `…/releases/<v>/bin/codex` runs siblings (`codex-code-mode-host`)
                // and resources (`codex-path/rg`) from the release directory.
                install_levels: 2,
                auth_files: &[".codex/auth.json"],
                strip_keys: &[],
                vendor_dirs: &[".codex"],
                home_skip: &[
                    "packages",
                    "sessions",
                    "log",
                    "logs",
                    "history.jsonl",
                    "tmp",
                    "shell_snapshots",
                ],
                hosts: &[
                    "chatgpt.com",
                    "auth.openai.com",
                    "api.openai.com",
                    // OpenAI's content CDN (`sdmntpr*.oaiusercontent.com`), fetched at
                    // startup; left to prompt, every codex job would ask.
                    "oaiusercontent.com",
                ],
                env: &[("CODEX_DISABLE_UPDATE_CHECK", "1")],
            },
            HarnessKind::Agy => KindSpec {
                binary: "agy",
                install_levels: 0,
                auth_files: &[".gemini/antigravity-cli/antigravity-oauth-token"],
                strip_keys: &[],
                vendor_dirs: &[".gemini/antigravity-cli"],
                home_skip: &[
                    "bin",
                    "brain",
                    "cache",
                    "conversations",
                    "conversation_summaries.db",
                    "crashes",
                    "history.jsonl",
                    "log",
                    "cli.log",
                    "scratch",
                    "updater",
                    "presence",
                ],
                hosts: &[
                    "daily-cloudcode-pa.googleapis.com",
                    "cloudcode-pa.googleapis.com",
                    "oauth2.googleapis.com",
                    "accounts.google.com",
                    "www.googleapis.com",
                    "antigravity.google.com",
                    // Its startup "eligibility check" fetches the account's profile
                    // picture and fails the run if it cannot.
                    "lh3.googleusercontent.com",
                    // Its feature-flag service, contacted at startup; left to prompt,
                    // every agy job would ask before doing anything.
                    "antigravity-unleash.goog",
                ],
                env: &[],
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::Grok => "grok",
            HarnessKind::Claude => "claude",
            HarnessKind::Codex => "codex",
            HarnessKind::Agy => "agy",
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
