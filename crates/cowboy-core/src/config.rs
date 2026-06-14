//! Configuration model for cowboy.
//!
//! Three files live under `.cowboy/`:
//!
//! * [`SecurityConfig`] (`security.yaml`) — **host-owned**, never mounted into
//!   the agent container. Controls the container, mounts, networks, network
//!   policy, and secret injection.
//! * [`AgentConfig`] (`agent.yaml`) — mounted into the container, agent-editable.
//!   Non-security behavior only (timeouts, processes, command shortcuts).
//! * [`ModelsConfig`] (`models.yaml`) — host-owned model profiles for the
//!   OpenAI-compatible client.
//!
//! Loaders enforce security invariants (see [`SecurityConfig::validate`]): the
//! agent must never be able to mount `security.yaml`, and dangerous options are
//! surfaced rather than silently honored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The directory, relative to the project root, that holds cowboy config.
pub const COWBOY_DIR: &str = ".cowboy";
/// Host-owned security config filename. Never mounted into the container.
pub const SECURITY_FILE: &str = "security.yaml";
/// Agent-visible config filename. Mounted into the container.
pub const AGENT_FILE: &str = "agent.yaml";
/// Model profiles filename.
pub const MODELS_FILE: &str = "models.yaml";

// ---------------------------------------------------------------------------
// security.yaml
// ---------------------------------------------------------------------------

/// Host-owned security configuration. This file is read only by the host
/// `cowboy` process and is **never** mounted into the agent container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub container: ContainerConfig,
    #[serde(default)]
    pub networks: NetworksConfig,
    #[serde(default)]
    pub network_policy: NetworkPolicy,
    #[serde(default)]
    pub secrets: SecretsConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerConfig {
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub build: bool,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    #[serde(default = "default_mounts")]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub docker_socket: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    #[serde(default = "default_mount_mode")]
    pub mode: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworksConfig {
    #[serde(default)]
    pub isolated: IsolatedNetwork,
    #[serde(default)]
    pub compose: ComposeNetworks,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IsolatedNetwork {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ComposeNetworks {
    /// Docker network names the user has approved the agent to join.
    #[serde(default)]
    pub approved: Vec<String>,
}

/// Default verdict applied to a class of destination when no explicit
/// allow/deny rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultVerdict {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkPolicy {
    #[serde(default = "default_ask")]
    pub default_external: DefaultVerdict,
    #[serde(default = "default_ask")]
    pub default_private_lan: DefaultVerdict,
    #[serde(default = "default_ask")]
    pub default_host: DefaultVerdict,
    #[serde(default)]
    pub allow: RuleSet,
    #[serde(default = "default_deny_rules")]
    pub deny: RuleSet,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub cidrs: Vec<String>,
    #[serde(default)]
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SecretsConfig {
    #[serde(default)]
    pub env: Vec<SecretEnv>,
}

/// A single secret env var injected into the container from a host env var.
/// The agent cannot edit this; values are never logged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretEnv {
    /// Name of the env var as seen inside the container.
    pub name: String,
    /// Name of the host env var to read the value from.
    pub source_env: String,
    #[serde(default)]
    pub required: bool,
    /// If `Some("required")`, injecting this secret needs explicit approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
}

// ---------------------------------------------------------------------------
// agent.yaml
// ---------------------------------------------------------------------------

/// Agent-visible configuration, mounted into the container and editable by the
/// agent. Contains no security controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub agent: AgentBehavior,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub processes: BTreeMap<String, ProcessDef>,
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentBehavior {
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: u64,
    #[serde(default = "default_model_timeout")]
    pub model_timeout_seconds: u64,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_max_output")]
    pub max_command_output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_scratchpad")]
    pub scratchpad: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessDef {
    pub command: String,
    #[serde(default = "default_workdir")]
    pub cwd: String,
    #[serde(default)]
    pub auto_start: bool,
}

// ---------------------------------------------------------------------------
// models.yaml
// ---------------------------------------------------------------------------

/// Model profiles for the OpenAI-compatible client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub models: ModelSet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSet {
    /// Name of the default profile.
    pub default: String,
    pub profiles: BTreeMap<String, ModelProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    pub base_url: String,
    /// Name of the env var holding the API key (never the key itself).
    pub api_key_env: String,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// defaults
// ---------------------------------------------------------------------------

fn default_version() -> u32 {
    1
}
fn default_true() -> bool {
    true
}
fn default_image() -> String {
    "cowboy/agent:local".to_string()
}
fn default_workdir() -> String {
    "/workspace".to_string()
}
fn default_mount_mode() -> String {
    "rw".to_string()
}
fn default_mounts() -> Vec<Mount> {
    vec![Mount {
        source: ".".to_string(),
        target: "/workspace".to_string(),
        mode: "rw".to_string(),
    }]
}
fn default_ask() -> DefaultVerdict {
    DefaultVerdict::Ask
}
fn default_command_timeout() -> u64 {
    600
}
fn default_model_timeout() -> u64 {
    120
}
fn default_max_iterations() -> u32 {
    100
}
fn default_max_output() -> usize {
    60_000
}
fn default_scratchpad() -> String {
    ".cowboy/sessions/current/scratchpad.md".to_string()
}
fn default_temperature() -> f32 {
    0.2
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_context_window() -> u32 {
    200_000
}
fn default_deny_rules() -> RuleSet {
    RuleSet {
        domains: vec!["metadata.google.internal".to_string()],
        cidrs: vec![
            "169.254.169.254/32".to_string(),
            "100.100.100.200/32".to_string(),
        ],
        ports: vec![],
    }
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            image: default_image(),
            dockerfile: None,
            build: false,
            workdir: default_workdir(),
            mounts: default_mounts(),
            privileged: false,
            docker_socket: false,
            memory: None,
            cpus: None,
        }
    }
}

impl Default for IsolatedNetwork {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            default_external: DefaultVerdict::Ask,
            default_private_lan: DefaultVerdict::Ask,
            default_host: DefaultVerdict::Ask,
            allow: RuleSet::default(),
            deny: default_deny_rules(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            version: 1,
            container: ContainerConfig::default(),
            networks: NetworksConfig::default(),
            network_policy: NetworkPolicy::default(),
            secrets: SecretsConfig::default(),
        }
    }
}

impl Default for AgentBehavior {
    fn default() -> Self {
        Self {
            command_timeout_seconds: default_command_timeout(),
            model_timeout_seconds: default_model_timeout(),
            max_iterations: default_max_iterations(),
            max_command_output_bytes: default_max_output(),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            scratchpad: default_scratchpad(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            version: 1,
            agent: AgentBehavior::default(),
            session: SessionConfig::default(),
            processes: BTreeMap::new(),
            commands: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// loading & validation
// ---------------------------------------------------------------------------

/// Resolved paths to the three config files for a project rooted at `root`.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub dir: PathBuf,
    pub security: PathBuf,
    pub agent: PathBuf,
    pub models: PathBuf,
}

impl ConfigPaths {
    pub fn for_root(root: impl AsRef<Path>) -> Self {
        let dir = root.as_ref().join(COWBOY_DIR);
        Self {
            security: dir.join(SECURITY_FILE),
            agent: dir.join(AGENT_FILE),
            models: dir.join(MODELS_FILE),
            dir,
        }
    }
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Err(Error::ConfigNotFound(path.to_path_buf()));
    }
    let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    serde_yaml_ng::from_str(&text).map_err(|source| Error::ConfigParse {
        path: path.to_path_buf(),
        source,
    })
}

impl SecurityConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let cfg: Self = read_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Enforce the non-negotiable security invariants. Returns an error rather
    /// than silently honoring a dangerous configuration.
    pub fn validate(&self) -> Result<()> {
        for mount in &self.container.mounts {
            if mount_targets_security_file(&mount.source) {
                return Err(Error::SecurityInvariant(format!(
                    "mount source {:?} would expose the host-owned security config to the agent; \
                     security.yaml must never be mounted into the container",
                    mount.source
                )));
            }
        }
        Ok(())
    }

    /// Returns warnings for dangerous-but-permitted options. The host process
    /// should surface these to the user; they do not block startup.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.container.privileged {
            out.push("container.privileged = true grants the agent broad host access".to_string());
        }
        if self.container.docker_socket {
            out.push(
                "container.docker_socket = true exposes the Docker daemon to the agent (container escape risk)"
                    .to_string(),
            );
        }
        out
    }
}

/// True if a mount source path points at the host-owned security config.
fn mount_targets_security_file(source: &str) -> bool {
    let p = Path::new(source);
    if p.file_name().and_then(|n| n.to_str()) == Some(SECURITY_FILE) {
        return true;
    }
    // Also reject mounting the whole `.cowboy` dir, which would include it.
    p.file_name().and_then(|n| n.to_str()) == Some(COWBOY_DIR)
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self> {
        read_yaml(path)
    }
}

impl ModelsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let cfg: Self = read_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.models.profiles.contains_key(&self.models.default) {
            return Err(Error::Invalid(format!(
                "default model profile {:?} is not defined in profiles",
                self.models.default
            )));
        }
        Ok(())
    }

    /// Resolve the active profile (`name` or the configured default).
    pub fn resolve(&self, name: Option<&str>) -> Result<&ModelProfile> {
        let key = name.unwrap_or(&self.models.default);
        self.models
            .profiles
            .get(key)
            .ok_or_else(|| Error::Invalid(format!("unknown model profile: {key}")))
    }
}

// ---------------------------------------------------------------------------
// templates for `cowboy init`
// ---------------------------------------------------------------------------

/// Default `security.yaml` rendered by `cowboy init`, with comments.
pub fn security_template() -> String {
    SECURITY_TEMPLATE.to_string()
}
/// Default `agent.yaml` rendered by `cowboy init`, with comments.
pub fn agent_template() -> String {
    AGENT_TEMPLATE.to_string()
}
/// Default `models.yaml` rendered by `cowboy init`, with comments.
pub fn models_template() -> String {
    MODELS_TEMPLATE.to_string()
}

const SECURITY_TEMPLATE: &str = r#"version: 1

# HOST-OWNED security config. The cowboy host process reads this; it is NEVER
# mounted into the agent container. The agent cannot see or edit this file.

container:
  image: cowboy/agent:local
  # dockerfile: ./Dockerfile.cowboy
  build: false
  workdir: /workspace
  mounts:
    - source: .
      target: /workspace
      mode: rw
  privileged: false
  docker_socket: false
  memory: 8g
  cpus: 4

networks:
  isolated:
    enabled: true
  compose:
    approved: []

network_policy:
  default_external: ask
  default_private_lan: ask
  default_host: ask
  allow:
    domains:
      - github.com
      - api.github.com
      - crates.io
      - static.crates.io
      - index.crates.io
    cidrs: []
    ports:
      - 80
      - 443
  deny:
    domains:
      - metadata.google.internal
    cidrs:
      - 169.254.169.254/32
      - 100.100.100.200/32

secrets:
  env: []
    # - name: GITHUB_TOKEN
    #   source_env: COWBOY_GITHUB_TOKEN
    #   required: false
    #   approval: required
"#;

const AGENT_TEMPLATE: &str = r#"version: 1

# Agent-visible config. This IS mounted into the container and the agent may
# edit it. It contains no security controls.

agent:
  command_timeout_seconds: 600
  model_timeout_seconds: 120
  max_iterations: 100
  max_command_output_bytes: 60000

session:
  scratchpad: .cowboy/sessions/current/scratchpad.md

processes: {}
  # web:
  #   command: npm run dev
  #   cwd: /workspace
  #   auto_start: false

commands: {}
  # test: cargo test
  # lint: cargo clippy
"#;

const MODELS_TEMPLATE: &str = r#"version: 1

# Model profiles for the OpenAI-compatible client. The API key is read from the
# env var named by `api_key_env` — never store the key in this file.

models:
  default: dev
  profiles:
    dev:
      base_url: https://litellm-test.follow-chinstrap.ts.net/v1
      api_key_env: COWBOY_OPENAI_API_KEY
      model: anthropic/claude-sonnet-4-6
      temperature: 0.2
      max_tokens: 8192
      context_window: 200000
      headers: {}
    cheap:
      base_url: https://litellm-test.follow-chinstrap.ts.net/v1
      api_key_env: COWBOY_OPENAI_API_KEY
      model: openai/gpt-5.4-mini
      temperature: 0.1
      max_tokens: 4096
      context_window: 128000
      headers: {}
"#;
