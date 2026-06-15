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
/// Model definitions filename (home + project).
pub const MODELS_FILE: &str = "models.yaml";
/// Home-only providers filename (endpoint + key). Never in a project.
pub const PROVIDERS_FILE: &str = "providers.yaml";

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
    /// Host credential files/dirs granted (read-only by default) into the
    /// container so the agent can use CLIs like `gh`/`gcloud`/`kubectl`.
    #[serde(default)]
    pub files: Vec<SecretMount>,
}

/// A host credential path granted into the container. The agent cannot edit this
/// grant (security.yaml is host-owned and masked); only the user elects it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretMount {
    /// Host path (a leading `~` and `${VAR}` are expanded), e.g. `~/.config/gh`.
    pub source: String,
    /// Container path the credential is mounted at, e.g. `/tmp/.config/gh`
    /// (the container `HOME` is `/tmp`, where CLIs look).
    pub target: String,
    /// Mount read-only (the default; protects the host credential).
    #[serde(default = "default_true")]
    pub read_only: bool,
    /// Fail to start if the host source is missing (default: skip when absent).
    #[serde(default)]
    pub required: bool,
    /// If `Some("required")` (or `"ask"`), mounting this credential needs the
    /// user's explicit per-session approval before it is exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
}

impl SecretMount {
    /// Whether mounting this credential requires explicit per-session approval.
    pub fn needs_approval(&self) -> bool {
        approval_required(&self.approval)
    }
}

/// Whether an `approval` field opts a grant into a per-session approval prompt.
pub fn approval_required(approval: &Option<String>) -> bool {
    matches!(
        approval.as_deref(),
        Some("required") | Some("ask") | Some("yes") | Some("true")
    )
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

impl SecretEnv {
    /// Whether injecting this secret requires explicit per-session approval.
    pub fn needs_approval(&self) -> bool {
        approval_required(&self.approval)
    }
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
    /// Stop the session once total (input+output) tokens reach this many
    /// (0 = no limit). A soft warning fires at 80%.
    #[serde(default)]
    pub token_budget: u64,
    /// Stop the session once estimated model spend reaches this many USD
    /// (0 = no limit; requires the model's pricing to be known). Warns at 80%.
    #[serde(default)]
    pub cost_budget_usd: f64,
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
// providers.yaml (home-only) + models.yaml (home + project)
// ---------------------------------------------------------------------------

/// Model providers: endpoint + API key pairs. **Host-owned and home-only** —
/// this file lives at `~/.config/cowboy/providers.yaml` (mode `0600`) and is
/// never placed in a project or mounted into the agent container, so the agent
/// cannot reach the credentials by construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            providers: BTreeMap::new(),
        }
    }
}

/// A single OpenAI-compatible provider: where to send requests and the key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Endpoint base URL (supports `${VAR}` expansion from the host env).
    pub base_url: String,
    /// The API key, stored literally (the file is `0600`, home-owned).
    pub api_key: String,
    /// Optional default headers (e.g. for an internal gateway).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Model definitions. Lives at both the user level
/// (`~/.config/cowboy/models.yaml`) and the project level
/// (`.cowboy/models.yaml`); project entries override user entries by name and a
/// project may override the default. **Never contains provider credentials** —
/// `deny_unknown_fields` makes a stray `api_key`/`base_url`/`providers` a hard
/// parse error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Name of the default model (optional at the project level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelDef>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            default: None,
            models: BTreeMap::new(),
        }
    }
}

/// How hard a reasoning model should think. Sent as `reasoning_effort` in the
/// chat request; absent means the parameter is omitted entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// The wire value (also the user-facing label).
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

/// A named model: which provider to use plus model id and sampling params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    /// Name of the provider (looked up in `providers.yaml`).
    pub provider: String,
    /// The provider-side model id, e.g. `anthropic/claude-sonnet-4-6`.
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    /// Reasoning effort for reasoning models (omitted when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Nucleus sampling (config-file only; omitted when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Stop sequences (config-file only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Arbitrary extra request-body params merged in (config-file escape hatch).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
    /// Per-model header overrides (merged over the provider's headers).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// USD per 1M input (prompt) tokens, for cost estimation (omitted when unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cost_per_mtok: Option<f64>,
    /// USD per 1M output (completion) tokens, for cost estimation (omitted when unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cost_per_mtok: Option<f64>,
    /// Opt in to Anthropic prompt caching: Cowboy adds `cache_control` markers to
    /// the (static) system prompt and the latest message so a compatible gateway
    /// caches the prefix. Only enable for Anthropic models behind a gateway that
    /// understands `cache_control`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub anthropic_cache: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A fully-resolved model ready to build a client from: provider credentials
/// merged with the model definition. Decouples the client from the on-disk
/// layout.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub context_window: u32,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub top_p: Option<f32>,
    pub stop: Vec<String>,
    pub extra: BTreeMap<String, serde_json::Value>,
    pub headers: BTreeMap<String, String>,
    pub input_cost_per_mtok: Option<f64>,
    pub output_cost_per_mtok: Option<f64>,
    pub anthropic_cache: bool,
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

/// Default allow-list: common dev package registries on 80/443. Domains are
/// suffix-matched, so base domains cover their subdomains (e.g. `npmjs.org`
/// matches `registry.npmjs.org`).
fn default_allow_rules() -> RuleSet {
    RuleSet {
        domains: [
            "github.com",
            "githubusercontent.com",
            "crates.io",
            "npmjs.org",
            "pypi.org",
            "pythonhosted.org",
            "golang.org",
            "go.dev",
            "rubygems.org",
            "debian.org",
            "ghcr.io",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        cidrs: vec![],
        ports: vec![80, 443],
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
            allow: default_allow_rules(),
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
            token_budget: 0,
            cost_budget_usd: 0.0,
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
        // Credential grants: never re-expose host config, and never shadow the
        // workspace or the masked `.cowboy/` config with a mount target.
        let workdir = self.container.workdir.trim_end_matches('/');
        for grant in &self.secrets.files {
            if mount_targets_security_file(&grant.source) {
                return Err(Error::SecurityInvariant(format!(
                    "credential grant source {:?} would expose the host-owned security config",
                    grant.source
                )));
            }
            let target = grant.target.trim_end_matches('/');
            if !target.starts_with('/') {
                return Err(Error::SecurityInvariant(format!(
                    "credential grant target {:?} must be an absolute container path",
                    grant.target
                )));
            }
            if target == workdir || target.starts_with(&format!("{workdir}/")) {
                return Err(Error::SecurityInvariant(format!(
                    "credential grant target {:?} must be outside the workspace ({workdir}); \
                     it must not shadow the project or the masked config",
                    grant.target
                )));
            }
        }
        Ok(())
    }

    /// Serialize and write back to `path`. Note: this rewrites the file and
    /// does not preserve comments — used after an interactive approval updates
    /// `networks.compose.approved`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let yaml = serde_yaml_ng::to_string(self).map_err(|e| Error::Invalid(e.to_string()))?;
        std::fs::write(path, yaml).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })
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

/// The home config directory (`~/.config/cowboy`), if resolvable. Mirrors the
/// skills crate's use of `directories::BaseDirs`.
pub fn global_config_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("cowboy"))
}

fn write_yaml<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::ConfigRead {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let yaml = serde_yaml_ng::to_string(value).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::write(path, yaml).map_err(|source| Error::ConfigRead {
        path: path.to_path_buf(),
        source,
    })
}

impl ProvidersConfig {
    /// Load a providers file from a specific path.
    pub fn load(path: &Path) -> Result<Self> {
        read_yaml(path)
    }

    /// The home-only providers file (`~/.config/cowboy/providers.yaml`).
    pub fn global_path() -> Option<PathBuf> {
        global_config_dir().map(|d| d.join(PROVIDERS_FILE))
    }

    /// Load the home providers file, or an empty config if it doesn't exist.
    pub fn load_global() -> Result<Self> {
        match Self::global_path() {
            Some(p) if p.exists() => read_yaml(&p),
            _ => Ok(Self::default()),
        }
    }

    /// Write to `path` with owner-only (`0600`) permissions — this file holds
    /// API keys.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_yaml(self, path)?;
        set_owner_only(path)
    }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        }
    })
}
#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

impl ModelsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        read_yaml(path)
    }

    /// The user-level models file (`~/.config/cowboy/models.yaml`).
    pub fn user_path() -> Option<PathBuf> {
        global_config_dir().map(|d| d.join(MODELS_FILE))
    }

    /// Load a models file if it exists, else `None` (a missing file is not an
    /// error — user/project model lists are both optional).
    pub fn load_opt(path: &Path) -> Result<Option<Self>> {
        if path.exists() {
            Ok(Some(read_yaml(path)?))
        } else {
            Ok(None)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_yaml(self, path)
    }
}

/// Resolve the active model into a [`ResolvedModel`] by merging user and project
/// model lists (project overrides by name) and joining with the named provider's
/// credentials.
///
/// Default precedence: explicit `name` → project `default` → user `default`.
pub fn resolve_model(
    providers: &ProvidersConfig,
    user: Option<&ModelsConfig>,
    project: Option<&ModelsConfig>,
    name: Option<&str>,
) -> Result<ResolvedModel> {
    // Merge model definitions: user first, then project overrides by name.
    let mut models: BTreeMap<String, ModelDef> = BTreeMap::new();
    if let Some(u) = user {
        models.extend(u.models.clone());
    }
    if let Some(p) = project {
        models.extend(p.models.clone());
    }
    if models.is_empty() {
        return Err(Error::Invalid(
            "no models configured; run `cowboy models setup`".to_string(),
        ));
    }

    // Default precedence: explicit name, then project default, then user default.
    let chosen = name
        .map(str::to_string)
        .or_else(|| project.and_then(|p| p.default.clone()))
        .or_else(|| user.and_then(|u| u.default.clone()))
        .ok_or_else(|| {
            Error::Invalid(
                "no default model set; pick one with `cowboy models use <name>`".to_string(),
            )
        })?;

    let def = models
        .get(&chosen)
        .ok_or_else(|| Error::Invalid(format!("unknown model: {chosen}")))?;

    let provider = providers.providers.get(&def.provider).ok_or_else(|| {
        Error::Invalid(format!(
            "model {chosen:?} references provider {:?}, which is not configured; \
             run `cowboy models setup`",
            def.provider
        ))
    })?;

    // Provider headers first, then per-model overrides win.
    let mut headers = provider.headers.clone();
    headers.extend(def.headers.clone());

    Ok(ResolvedModel {
        base_url: expand_env(&provider.base_url)?,
        api_key: provider.api_key.clone(),
        model: def.model.clone(),
        temperature: def.temperature,
        max_tokens: def.max_tokens,
        context_window: def.context_window,
        reasoning_effort: def.reasoning_effort,
        top_p: def.top_p,
        stop: def.stop.clone(),
        extra: def.extra.clone(),
        headers,
        input_cost_per_mtok: def.input_cost_per_mtok,
        output_cost_per_mtok: def.output_cost_per_mtok,
        anthropic_cache: def.anthropic_cache,
    })
}

/// Expand `${VAR}` references in `input` from the host environment. Errors if a
/// referenced variable is unset or empty (so a misconfigured endpoint fails
/// loudly rather than silently pointing at an empty URL). Literal text and `$`
/// not followed by `{` are passed through unchanged.
pub fn expand_env(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            Error::Invalid(format!("unterminated `${{` in config value: {input:?}"))
        })?;
        let var = &after[..end];
        let value = std::env::var(var).unwrap_or_default();
        if value.is_empty() {
            return Err(Error::Invalid(format!(
                "config references ${{{var}}} but ${var} is unset or empty"
            )));
        }
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Expand a host path for a credential grant: a leading `~` becomes the home
/// directory, and `${VAR}` references are expanded (erroring if unset).
pub fn expand_path(input: &str) -> Result<PathBuf> {
    let expanded = expand_env(input)?;
    if expanded == "~" {
        if let Some(b) = directories::BaseDirs::new() {
            return Ok(b.home_dir().to_path_buf());
        }
    } else if let Some(rest) = expanded.strip_prefix("~/") {
        if let Some(b) = directories::BaseDirs::new() {
            return Ok(b.home_dir().join(rest));
        }
    }
    Ok(PathBuf::from(expanded))
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
    # Domains are suffix-matched (npmjs.org also matches registry.npmjs.org).
    domains:
      - github.com
      - githubusercontent.com
      - crates.io
      - npmjs.org
      - pypi.org
      - pythonhosted.org
      - golang.org
      - go.dev
      - rubygems.org
      - debian.org
      - ghcr.io
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
  # Env vars injected from the host (values read at runtime, never stored here).
  env: []
    # - name: GITHUB_TOKEN
    #   source_env: COWBOY_GITHUB_TOKEN
    #   required: false
    #   approval: required
  # Host credential files/dirs granted (read-only by default) into the container
  # so the agent can use CLIs like gh/gcloud/kubectl. The container HOME is /tmp,
  # so mount under /tmp/... where the tools look. `cowboy secrets add <preset>`
  # prints ready-to-paste entries. You must also allow the matching network host.
  files: []
    # - source: ~/.config/gh
    #   target: /tmp/.config/gh
    #   read_only: true
    #   required: false
    #   approval: required   # prompt for per-session approval before mounting
"#;

const AGENT_TEMPLATE: &str = r#"version: 1

# Agent-visible config. This IS mounted into the container and the agent may
# edit it. It contains no security controls.

agent:
  command_timeout_seconds: 600
  model_timeout_seconds: 120
  max_iterations: 100
  max_command_output_bytes: 60000
  # Optional usage budgets (0 = no limit). The session stops once a budget is
  # reached, with a soft warning at 80%. The cost estimate uses the model's
  # per-token pricing (see `cowboy models` / model-defaults).
  # token_budget: 0
  # cost_budget_usd: 0.0

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
