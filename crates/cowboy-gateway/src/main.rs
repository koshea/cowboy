//! cowboy-gateway: the sole-egress network gateway binary baked into the
//! gateway container image. Enforces allow/deny/ask network policy for the
//! untrusted agent container.
//!
//! Startup is fail-closed: if the nft ruleset cannot be applied, the gateway
//! refuses to run rather than degrade into an open router.

mod config;
mod control;
mod dns;
mod dns_policy;
mod http;
mod nft;
mod proxy;
mod sni;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;

use crate::config::{GatewayConfig, PORT_DNS};
use crate::control::ControlClient;
use crate::dns::DnsMap;
use crate::state::GatewayState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("COWBOY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = GatewayConfig::from_env()?;
    tracing::info!(
        subnet = %cfg.agent_subnet,
        upstream = %cfg.dns_upstream,
        control = ?cfg.control_addr,
        "cowboy-gateway starting"
    );

    // Load-bearing enforcement. Fail closed if this errors.
    nft::apply(&cfg).await?;

    let control = ControlClient::new(cfg.control_addr.clone(), cfg.control_token.clone());
    let state = Arc::new(GatewayState::new(
        cfg.policy.clone(),
        DnsMap::new(),
        control,
    ));

    // DNS resolver (policy-enforced). Bind loopback, not 0.0.0.0: the agent's
    // queries are REDIRECTed here (DNAT to 127.0.0.1:PORT_DNS), and replying from
    // 127.0.0.1 is what lets conntrack reverse the DNAT so the agent accepts the
    // answer (a 0.0.0.0 bind would source the reply from the netns IP and the
    // agent would reject it).
    let dns_bind: SocketAddr = ([127, 0, 0, 1], PORT_DNS).into();
    let upstream = cfg.dns_upstream;
    let dns_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = dns::serve(dns_bind, upstream, dns_state).await {
            tracing::error!(error = %e, "dns resolver exited");
        }
    });

    // Proxy listeners (runs until a listener fails).
    proxy::run(state).await
}
