//! Configuration model for cowboy.
//!
//! Three files live under `.cowboy/`:
//!
//! * [`SecurityConfig`] (`security.yaml`) — **host-owned**, never mounted into
//!   the sandbox. Controls what the sandbox can see, its resource ceilings, network
//!   policy, and secret injection.
//! * [`AgentConfig`] (`agent.yaml`) — visible in the sandbox, agent-editable.
//!   Non-security behavior only (timeouts, processes, command shortcuts).
//! * [`ModelsConfig`] (`models.yaml`) — host-owned model profiles for the
//!   OpenAI-compatible client.
//!
//! Loaders enforce security invariants (see [`SecurityConfig::validate`]): the agent
//! must never be able to mount `security.yaml`, a config still using the
//! pre-sandbox `container:` key is refused by name rather than silently ignored, and
//! an unknown mount mode fails closed instead of defaulting to writable.

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
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Set only when a config still uses the pre-sandbox `container:` key.
    ///
    /// Kept solely so [`SecurityConfig::validate`] can refuse it by name. Ignoring an
    /// unknown section would be the dangerous outcome: the mounts under it would
    /// silently vanish, leaving the agent with only the default workspace mount and
    /// no indication why its paths disappeared.
    #[serde(default, skip_serializing, rename = "container")]
    pub legacy_container: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub network_policy: NetworkPolicy,
    #[serde(default)]
    pub secrets: SecretsConfig,
}

/// How the agent's sandbox is shaped: what it can see, and how much of the machine
/// it may use.
///
/// Deliberately small. `image`, `dockerfile`, `build`, `privileged` and
/// `docker_socket` are gone with the container — none of them described anything a
/// sandbox does, and `privileged`/`docker_socket` in particular were only ever
/// honoured as warnings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Where the project appears inside the sandbox.
    #[serde(default = "default_workdir")]
    pub workdir: String,
    /// Host paths exposed inside it. For one-off access prefer `cowboy grant`, which
    /// takes effect on the next command without editing this file.
    #[serde(default = "default_mounts")]
    pub mounts: Vec<Mount>,
    /// Memory ceiling (e.g. `8g`), or `auto` to size from the host. None = unlimited.
    /// Enforced with a cgroup v2 `memory.max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// CPU ceiling: a number (e.g. `2`) or `auto` (sized from the host). Enforced with
    /// a cgroup v2 `cpu.max`, and also bounds build parallelism — `-j{cpus}` build env
    /// is injected, because not every tool reads the quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<CpuLimit>,
    /// Expose the user's own tool directories (`~/.local/bin`, `~/.cargo/bin` and
    /// friends) read-only, so the agent runs the same tools the user does.
    ///
    /// On by default: `/usr` alone means the agent silently has a *different*
    /// toolchain from the person directing it — a different `cargo`, and none of the
    /// things installed with `pipx`, `uv tool`, `npm -g --prefix=~/.local`, `go
    /// install` or `cargo install`. Set false for a sandbox that sees only what the
    /// system package manager put on the machine.
    #[serde(default = "default_true")]
    pub host_tools: bool,
}

/// A CPU limit: an explicit core count, or `auto` (resolved from the host).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CpuLimit {
    Auto,
    Cores(f64),
}

impl Serialize for CpuLimit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            CpuLimit::Auto => s.serialize_str("auto"),
            CpuLimit::Cores(n) => s.serialize_f64(*n),
        }
    }
}

impl<'de> Deserialize<'de> for CpuLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // Accept either a number or the string "auto".
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Num(f64),
            Str(String),
        }
        match Repr::deserialize(d)? {
            Repr::Num(n) => Ok(CpuLimit::Cores(n)),
            Repr::Str(s) if s.eq_ignore_ascii_case("auto") => Ok(CpuLimit::Auto),
            Repr::Str(s) => Err(serde::de::Error::custom(format!(
                "cpus must be a number or \"auto\", got {s:?}"
            ))),
        }
    }
}

/// `auto` CPU limit from the host's logical core count: half the cores, clamped to
/// [2, 8] — leaves headroom and keeps build parallelism (and memory) bounded.
pub fn auto_cpus(host_cores: usize) -> f64 {
    ((host_cores / 2).clamp(2, 8)) as f64
}

/// `auto` memory limit (MiB) from the host's total RAM: a quarter, clamped to
/// [4 GiB, 16 GiB].
pub fn auto_mem_mib(host_total_mib: u64) -> u64 {
    (host_total_mib / 4).clamp(4096, 16384)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub source: String,
    pub target: String,
    #[serde(default = "default_mount_mode")]
    pub mode: String,
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
#[serde(deny_unknown_fields)]
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
    /// DNS resolution policy (strict allowlist gating + tunnel detection). Serde
    /// default keeps older configs/policy.json parsing.
    #[serde(default)]
    pub dns: DnsPolicy,
}

/// Policy for the gateway's DNS resolver. Defaults are the secure posture: strict
/// allowlist-gated resolution (only Allowed/approved names leave the gateway),
/// risky record types refused, and tunnel detection on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsPolicy {
    /// Apply the full allow/deny/default policy to each query name (resolve only
    /// Allowed/approved; REFUSE the rest locally). When false, the resolver only
    /// enforces the deny-list + tunnel detection and otherwise resolves freely.
    #[serde(default = "default_true")]
    pub enforce: bool,
    /// Record types allowed to resolve. Default omits the classic tunnel/C2
    /// carriers (TXT/NULL/ANY/AXFR/IXFR); add them here to opt in.
    #[serde(default = "default_allowed_qtypes")]
    pub allowed_qtypes: Vec<String>,
    /// Run tunnel-detection heuristics (high-entropy/long labels, query rate).
    #[serde(default = "default_true")]
    pub tunnel_detection: bool,
    /// Heuristic thresholds (sane defaults; rarely changed).
    #[serde(default = "default_max_label_len")]
    pub max_label_len: u8,
    #[serde(default = "default_max_qname_len")]
    pub max_qname_len: u16,
    /// Distinct subdomains per registrable parent per minute before a query is
    /// treated as suspicious (the strongest tunnel signal).
    #[serde(default = "default_max_subdomains_per_min")]
    pub max_subdomains_per_min: u32,
}

impl Default for DnsPolicy {
    fn default() -> Self {
        Self {
            enforce: true,
            allowed_qtypes: default_allowed_qtypes(),
            tunnel_detection: true,
            max_label_len: default_max_label_len(),
            max_qname_len: default_max_qname_len(),
            max_subdomains_per_min: default_max_subdomains_per_min(),
        }
    }
}

/// The default safe record-type allowlist (excludes TXT/NULL/ANY/AXFR/IXFR).
fn default_allowed_qtypes() -> Vec<String> {
    [
        "A", "AAAA", "CNAME", "MX", "NS", "PTR", "SOA", "SRV", "CAA", "HTTPS", "SVCB",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn default_max_label_len() -> u8 {
    40
}
fn default_max_qname_len() -> u16 {
    150
}
fn default_max_subdomains_per_min() -> u32 {
    40
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub cidrs: Vec<String>,
    #[serde(default)]
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// A single secret env var injected into the container. The value comes from a
/// host env var (`source_env`) or, for keyring-backed tools, the trimmed stdout
/// of a host command (`source_command`, e.g. `gh auth token`). The agent cannot
/// edit this; values are never logged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEnv {
    /// Name of the env var as seen inside the container.
    pub name: String,
    /// Name of the host env var to read the value from (empty if using
    /// `source_command`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_env: String,
    /// Host command whose stdout (trimmed) is the value. Run at session start on
    /// the host — handy for keyring-backed tokens (`gh auth token`,
    /// `gcloud auth print-access-token`). Takes precedence over `source_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_command: Option<String>,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct AgentBehavior {
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: u64,
    #[serde(default = "default_model_timeout")]
    pub model_timeout_seconds: u64,
    /// Stop a detached, idle session's container after this many seconds with no
    /// running turn and no attached client, to free its RAM (the next command
    /// restarts it). `0` disables idle teardown.
    /// Tear down an idle detached session's sandbox after this many seconds
    /// (`0` = never). The session stays resumable; the next command brings it back.
    ///
    /// The old name is accepted so a rename in non-security config does not break
    /// every project's `agent.yaml` for no benefit. Unlike the `container:` →
    /// `sandbox:` move in `security.yaml`, silently falling back to the default
    /// here costs a timeout, not a boundary.
    #[serde(
        default = "default_idle_sandbox_timeout",
        alias = "idle_container_timeout_seconds"
    )]
    pub idle_sandbox_timeout_seconds: u64,
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
    /// Repo setup commands, run in the container **once per worktree** (after
    /// `mise install`) when a session first comes up — e.g. `["mise run sync"]` to
    /// install all deps. Streamed to the UI; gated by a per-worktree marker so a
    /// second session in the same worktree skips them (delete
    /// `.cowboy/sessions/.worktree-setup` to force a re-run).
    #[serde(default)]
    pub setup: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    #[serde(default = "default_scratchpad")]
    pub scratchpad: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Name of the model used for auxiliary summarization — context compaction and
    /// truncation recovery — resolved like `default`. When unset, those calls fall
    /// back to the session's main model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizer: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelDef>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            default: None,
            summarizer: None,
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
    /// Abort a streaming response when the provider sends *nothing* (not even an
    /// SSE keep-alive) for this many seconds — a silent mid-stream stall would
    /// otherwise hang the session's turn forever. `0` disables; unset uses the
    /// client default (300s), generous enough for reasoning models that think
    /// without streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_idle_timeout_seconds: Option<u64>,
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
    pub stream_idle_timeout_seconds: Option<u64>,
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
fn default_idle_sandbox_timeout() -> u64 {
    1800 // 30 min: free a detached, idle session's container RAM (restarts on use)
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

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            workdir: default_workdir(),
            mounts: default_mounts(),
            memory: None,
            cpus: None,
            host_tools: true,
        }
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
            dns: DnsPolicy::default(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            version: 1,
            sandbox: SandboxConfig::default(),
            legacy_container: None,
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
            idle_sandbox_timeout_seconds: default_idle_sandbox_timeout(),
            max_iterations: default_max_iterations(),
            max_command_output_bytes: default_max_output(),
            token_budget: 0,
            cost_budget_usd: 0.0,
            setup: Vec::new(),
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
        if self.legacy_container.is_some() {
            return Err(Error::SecurityInvariant(
                "security.yaml uses `container:`, which is now `sandbox:`. The agent no \
                 longer runs in a container — rename the section, and delete `image`, \
                 `dockerfile`, `build`, `privileged` and `docker_socket`, which no \
                 longer do anything. `workdir`, `mounts`, `memory` and `cpus` carry \
                 over unchanged."
                    .to_string(),
            ));
        }
        for mount in &self.sandbox.mounts {
            if mount_targets_host_secret(&mount.source) {
                return Err(Error::SecurityInvariant(format!(
                    "mount source {:?} would expose host-owned secrets to the agent; \
                     security.yaml/providers.yaml and the cowboy config dir must never be mounted",
                    mount.source
                )));
            }
            // Fail closed on an unknown mode rather than silently treating it as
            // read-write (a typo like `readonly` must not yield a writable mount).
            if !matches!(mount.mode.as_str(), "ro" | "rw") {
                return Err(Error::SecurityInvariant(format!(
                    "mount mode {:?} is invalid; use \"ro\" or \"rw\"",
                    mount.mode
                )));
            }
        }
        // Credential grants: never re-expose host config, and never shadow the
        // workspace or the masked `.cowboy/` config with a mount target.
        let workdir = self.sandbox.workdir.trim_end_matches('/');
        for grant in &self.secrets.files {
            if mount_targets_host_secret(&grant.source) {
                return Err(Error::SecurityInvariant(format!(
                    "credential grant source {:?} would expose host-owned secrets \
                     (security.yaml/providers.yaml or the cowboy config dir)",
                    grant.source
                )));
            }
            let target = grant.target.trim_end_matches('/');
            if !target.starts_with('/') {
                return Err(Error::SecurityInvariant(format!(
                    "credential grant target {:?} must be an absolute path inside the sandbox",
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
    ///
    /// This used to flag `privileged` and `docker_socket`, which are gone with the
    /// container. The equivalent risk now is a **broad mount**: the denylist refuses
    /// credential stores and host-owned config outright, but a read-write mount of a
    /// whole home directory or of `/` is permitted and hands the agent most of the
    /// machine. Permitted because someone may genuinely mean it; surfaced because
    /// almost nobody does.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let home = expand_path("~").ok();
        for m in &self.sandbox.mounts {
            let Ok(source) = expand_path(&m.source) else {
                continue;
            };
            let broad = source.parent().is_none() // "/"
                || home.as_ref().is_some_and(|h| source == *h);
            if broad {
                out.push(format!(
                    "sandbox.mounts exposes {} ({}) — that is most of the machine. \
                     Prefer mounting the paths you need, or `cowboy grant <path>` as \
                     you go",
                    source.display(),
                    m.mode
                ));
            }
        }
        out
    }
}

/// True if `source` points at — or *contains* — a host-owned secret/config the
/// agent must never see: `security.yaml`, `providers.yaml` (API keys!), a project
/// `.cowboy` dir, or the home `cowboy` config dir. The check resolves `~`/`${VAR}`
/// and matches by path prefix, not just basename, so mounting an **ancestor** of
/// the config dir (e.g. `~` or `~/.config`) — which would drag `providers.yaml`
/// into the container — is also refused. Defense in depth: the agent can't author
/// `security.yaml`, but a user mustn't be able to foot-gun their keys in via a
/// mount/grant either.
fn mount_targets_host_secret(source: &str) -> bool {
    // Resolve ~ and ${VAR}; fall back to the literal path if expansion fails.
    let resolved = expand_path(source).unwrap_or_else(|_| PathBuf::from(source));
    let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);

    // The secret files by basename, or any `.cowboy` component in the path.
    let name = resolved.file_name().and_then(|n| n.to_str());
    if matches!(
        name,
        Some(SECURITY_FILE) | Some(PROVIDERS_FILE) | Some(COWBOY_DIR)
    ) {
        return true;
    }
    if resolved.components().any(|c| c.as_os_str() == COWBOY_DIR) {
        return true;
    }

    // The home cowboy config dir (holds providers.yaml): refuse the dir itself,
    // anything inside it, or any ancestor of it.
    if let Some(cfg_dir) = global_config_dir() {
        let cfg_dir = std::fs::canonicalize(&cfg_dir).unwrap_or(cfg_dir);
        if resolved.starts_with(&cfg_dir) || cfg_dir.starts_with(&resolved) {
            return true;
        }
    }
    false
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self> {
        read_yaml(path)
    }
}

/// A user base dir resolved the XDG way on *every* platform: the named XDG env
/// var if set, else `~/<fallback>`. Cowboy standardizes on the XDG layout — the
/// security model documents credentials at `~/.config/cowboy/providers.yaml`,
/// and the test suite isolates config via `XDG_CONFIG_HOME`. Plain
/// `directories::BaseDirs` would resolve to `~/Library/Application Support` on
/// macOS (and `%APPDATA%` on Windows), diverging from both; on Linux this is
/// identical to what `BaseDirs` already returned.
fn xdg_dir(env: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(env).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(v));
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join(fallback))
}

/// The home config directory (`~/.config/cowboy`), if resolvable.
pub fn global_config_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("cowboy"))
}

/// The home cache directory (`~/.cache/cowboy`), if resolvable. For data that's
/// expensive to rebuild but safe to lose — e.g. the mise toolchain store
/// persisted across agent-container recreations.
pub fn global_cache_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache").map(|d| d.join("cowboy"))
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
    /// API keys. The file is created `0600` *before* any bytes are written (via a
    /// temp file + atomic rename), so there's no window where the keys are
    /// world-readable.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_yaml_private(self, path)
    }

    /// Whether `path` (a providers file) is readable by group or other — i.e. its
    /// `0600` invariant has been broken (hand-edited, restored, copied). Used by
    /// `cowboy doctor` to surface a leaked-key risk. Always false on non-unix.
    pub fn perms_are_loose(path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o077 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    }
}

/// Host-global settings for the `cowboy web` remote-control server. The daemon
/// serves the web UI whenever `enabled`; toggled with `cowboy web on|off`. Lives
/// at `~/.config/cowboy/web.yaml` and holds the access token, so it's `0600`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct WebConfig {
    /// Whether `cowboyd` serves the web UI.
    pub enabled: bool,
    /// Bind address, e.g. `127.0.0.1:8787` or a Tailscale IP `100.x.y.z:8787`.
    pub bind: String,
    /// Bearer token clients must present. Minted on first `cowboy web on`.
    pub token: String,
    /// Permit a non-loopback, non-Tailscale bind (token travels in cleartext).
    pub allow_lan: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:8787".into(),
            token: String::new(),
            allow_lan: false,
        }
    }
}

impl WebConfig {
    /// `~/.config/cowboy/web.yaml`.
    pub fn global_path() -> Option<PathBuf> {
        global_config_dir().map(|d| d.join("web.yaml"))
    }

    /// Load the global web config, or defaults if absent/unreadable.
    pub fn load_global() -> Self {
        match Self::global_path() {
            Some(p) if p.exists() => read_yaml(&p).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    /// Persist to `~/.config/cowboy/web.yaml` with owner-only (`0600`) perms (it
    /// holds the access token).
    pub fn save_global(&self) -> Result<()> {
        let path =
            Self::global_path().ok_or_else(|| Error::Invalid("no home config dir".into()))?;
        write_yaml_private(self, &path)
    }

    /// The full access URL, or `None` until a token has been minted.
    pub fn url(&self) -> Option<String> {
        (!self.token.is_empty()).then(|| format!("http://{}/?token={}", self.bind, self.token))
    }
}

/// Serialize `value` to YAML at `path` with owner-only (`0600`) perms, created
/// atomically (temp file at `0600` + rename) so secrets are never briefly exposed.
fn write_yaml_private<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::ConfigRead {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let yaml = serde_yaml_ng::to_string(value).map_err(|e| Error::Invalid(e.to_string()))?;
    let err = |p: &Path, source| Error::ConfigRead {
        path: p.to_path_buf(),
        source,
    };
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("tmp");
        // Helper so any failure after the temp file exists removes it (no stale
        // 0600 leftover) before propagating the error.
        let cleanup = |e: Error| {
            let _ = std::fs::remove_file(&tmp);
            e
        };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|s| err(&tmp, s))?;
        f.write_all(yaml.as_bytes())
            .map_err(|s| cleanup(err(&tmp, s)))?;
        f.sync_all().map_err(|s| cleanup(err(&tmp, s)))?;
        std::fs::rename(&tmp, path).map_err(|s| cleanup(err(path, s)))?;
        // Durably commit the rename so a crash can't leave the file missing.
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, yaml).map_err(|s| err(path, s))?;
        set_owner_only(path)
    }
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
        stream_idle_timeout_seconds: def.stream_idle_timeout_seconds,
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
# visible inside the sandbox. The agent cannot see or edit this file.

sandbox:
  # Where the project appears inside the sandbox, and what else it can see.
  workdir: /workspace
  mounts:
    - source: .
      target: /workspace
      mode: rw
  # Add a path here to expose it permanently, or grant one as you go with
  # `cowboy grant <path>` — the sandbox picks it up on the next command, with no
  # restart. Credential stores (~/.aws, ~/.ssh, …) are always refused; use
  # `cowboy secrets add` for those.
  #
  # Expose your own tool directories read-only (~/.local/bin, ~/bin,
  # ~/.cargo/bin, ~/go/bin, plus the data dirs they resolve into) and put them on
  # PATH ahead of the system ones, so the agent runs the same tools you do. With
  # this off it sees only what your package manager installed — a different
  # `cargo`, and nothing from pipx / uv tool / go install / cargo install.
  host_tools: true
  # Resource ceilings, enforced with a cgroup. `cpus` also bounds build
  # parallelism: builds run with `-j{cpus}` (make/cargo/npm/cmake), because not
  # every tool reads the CPU quota. Use `auto` to size from the host
  # (cpus = half the cores [2..8]; memory = a quarter of RAM [4g..16g]).
  memory: 8g
  cpus: 2

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
  # DNS resolution policy. Defaults (shown) are the secure posture: the resolver
  # only forwards names the policy above Allows or you approve, refuses the rest
  # locally, blocks tunnel-prone record types, and prompts on suspected tunneling.
  dns:
    enforce: true              # apply the allow/deny/default policy to query names
    tunnel_detection: true     # prompt on high-entropy/long names or high query rate
    # Record types allowed to resolve (TXT/NULL/ANY/AXFR/IXFR are excluded by
    # default — the classic DNS-tunnel/C2 carriers; add them here to opt in).
    allowed_qtypes: [A, AAAA, CNAME, MX, NS, PTR, SOA, SRV, CAA, HTTPS, SVCB]
    # Heuristic thresholds (rarely changed):
    # max_label_len: 40
    # max_qname_len: 150
    # max_subdomains_per_min: 40

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
  # Stop a detached, idle session's container after this many seconds (no running
  # turn, no attached client) to free its RAM; the next command restarts it.
  # 0 disables. The container is *removed* outright when the session ends.
  idle_sandbox_timeout_seconds: 1800
  max_iterations: 100
  max_command_output_bytes: 60000
  # Optional usage budgets (0 = no limit). The session stops once a budget is
  # reached, with a soft warning at 80%. The cost estimate uses the model's
  # per-token pricing (see `cowboy models` / model-defaults).
  # token_budget: 0
  # cost_budget_usd: 0.0
  # Repo setup commands, run in the container once per worktree (after
  # `mise install`) when a session first comes up — e.g. to install all deps.
  # Streamed to the UI; a per-worktree marker skips them next time (delete
  # .cowboy/sessions/.worktree-setup to force a re-run).
  # setup:
  #   - mise run sync

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
