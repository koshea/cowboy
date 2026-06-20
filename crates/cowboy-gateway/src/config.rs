//! Gateway runtime configuration, supplied by the host via environment
//! variables and a bind-mounted policy file.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use cowboy_core::config::NetworkPolicy;

/// Listener ports inside the gateway container.
pub const PORT_CONNECT: u16 = 8080;
/// The single transparent proxy port: all of the agent's TCP is REDIRECTed here,
/// and the handler sniffs SNI/Host (any port) or falls back to the DNS map.
pub const PORT_TLS: u16 = 8443;
pub const PORT_DNS: u16 = 53;

/// Resolved gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Network policy (allow/deny/ask) resolved from the host security config.
    pub policy: NetworkPolicy,
    /// Source subnet of the agent network (e.g. `10.88.0.0/24`).
    pub agent_subnet: String,
    /// Upstream DNS resolver to forward queries to.
    pub dns_upstream: SocketAddr,
    /// Host control address (`host:port`) to dial for `ask` decisions, if
    /// available (None => fail-closed asks).
    pub control_addr: Option<String>,
    /// Per-session token presented to the host control server (must match, or the
    /// host drops the connection).
    pub control_token: Option<String>,
    /// Approved Docker/Compose subnets: exempt from the REDIRECT (bypass the
    /// proxy) and accepted by the filter-drop chain.
    pub allow_subnets: Vec<String>,
}

impl GatewayConfig {
    /// Load from environment. `COWBOY_POLICY_FILE` is required and holds a
    /// JSON-serialized [`NetworkPolicy`].
    pub fn from_env() -> Result<Self> {
        let policy_file =
            std::env::var("COWBOY_POLICY_FILE").context("COWBOY_POLICY_FILE not set")?;
        let text = std::fs::read_to_string(&policy_file)
            .with_context(|| format!("reading policy file {policy_file}"))?;
        let policy: NetworkPolicy = serde_json::from_str(&text).context("parsing policy JSON")?;

        let agent_subnet =
            std::env::var("COWBOY_AGENT_SUBNET").unwrap_or_else(|_| "10.88.0.0/24".to_string());
        // Forward approved queries to Docker's embedded resolver (always 127.0.0.11
        // on a user-defined network), not straight to a public resolver — that
        // preserves Docker Compose service-name discovery while still routing every
        // agent query through *this* resolver first (gating + IP→host recording).
        // cowboy's own forward is `skuid 0`-exempt, so it reaches 127.0.0.11.
        let dns_upstream = std::env::var("COWBOY_DNS_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.11:53".to_string())
            .parse()
            .context("parsing COWBOY_DNS_UPSTREAM")?;
        let control_addr = std::env::var("COWBOY_CONTROL_ADDR").ok();
        let control_token = std::env::var("COWBOY_CONTROL_TOKEN").ok();
        let allow_subnets = std::env::var("COWBOY_ALLOW_SUBNETS")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            policy,
            agent_subnet,
            dns_upstream,
            control_addr,
            control_token,
            allow_subnets,
        })
    }
}
