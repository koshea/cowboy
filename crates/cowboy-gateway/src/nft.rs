//! nftables ruleset for the gateway, running as a **sidecar in the agent's
//! network namespace**.
//!
//! The gateway shares the agent's netns, so the agent's outbound traffic is
//! *locally generated* and we intercept it in two hooks:
//!
//! - `nat output` REDIRECTs **all** of the agent's TCP to the in-process proxy
//!   (DNS to the resolver), which applies policy. The proxy sniffs each connection
//!   (SNI/Host on any port) and falls back to the DNS map for opaque traffic.
//! - `filter output` then **drops by default**, so the residue the REDIRECT can't
//!   carry — non-DNS UDP, ICMP — can't leak. This is what restores deny-by-default
//!   on every port/protocol.
//!
//! The gateway's own egress (the proxy's upstream dials, DNS forwarding, the host
//! control channel) is exempted by uid: it runs as root, the agent as the
//! unprivileged host uid, so `skuid 0` is the gateway and is left untouched (else
//! the proxy's own dial would be redirected back into itself). Approved Compose
//! subnets bypass the proxy and are accepted directly. Applying the ruleset is
//! fatal on failure — we fail closed, never open.

use anyhow::{bail, Context, Result};

use crate::config::{GatewayConfig, PORT_DNS, PORT_TLS};

/// The uid the gateway process runs as (root). Its own sockets are exempt from
/// the REDIRECT so relayed/upstream/control connections egress directly.
const GATEWAY_UID: u32 = 0;

/// Render the nft ruleset for the given config.
pub fn ruleset(cfg: &GatewayConfig) -> String {
    // Approved Compose/Docker networks bypass the proxy: not redirected in `nat`
    // and accepted in `filter`. Everything else is gated.
    let mut nat_exempt = String::new();
    let mut filter_allow = String::new();
    for net in &cfg.allow_subnets {
        nat_exempt.push_str(&format!("    ip daddr {net} return\n"));
        filter_allow.push_str(&format!("    ip daddr {net} accept\n"));
    }
    format!(
        "table ip cowboy {{
  chain output {{
    type nat hook output priority -100; policy accept;
    meta skuid {GATEWAY_UID} return
    ip daddr 127.0.0.0/8 return
{nat_exempt}    udp dport 53 redirect to :{PORT_DNS}
    tcp dport 53 redirect to :{PORT_DNS}
    meta l4proto tcp redirect to :{PORT_TLS}
  }}
  chain filter_out {{
    type filter hook output priority 0; policy drop;
    meta skuid {GATEWAY_UID} accept
    ip daddr 127.0.0.0/8 accept
    ct state established,related accept
{filter_allow}    udp dport 53 accept
    meta l4proto tcp accept
  }}
}}
"
    )
}

/// Apply the ruleset via `nft -f -`. Fatal on failure (fail-closed).
pub async fn apply(cfg: &GatewayConfig) -> Result<()> {
    let rules = ruleset(cfg);
    // Flush any prior cowboy table, then load.
    let script = format!("table ip cowboy\ndelete table ip cowboy\n{rules}");
    apply_script(&script)
        .await
        .context("applying nft ruleset (gateway fails closed if this fails)")
}

async fn apply_script(script: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning nft (is nftables installed in the gateway image?)")?;

    child
        .stdin
        .as_mut()
        .context("nft stdin")?
        .write_all(script.as_bytes())
        .await?;
    // Close stdin so nft processes the script.
    drop(child.stdin.take());

    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "nft exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::NetworkPolicy;

    fn cfg() -> GatewayConfig {
        GatewayConfig {
            policy: NetworkPolicy::default(),
            agent_subnet: "10.88.0.0/24".into(),
            dns_upstream: "1.1.1.1:53".parse().unwrap(),
            control_addr: None,
            control_token: None,
            allow_subnets: vec!["172.20.0.0/16".into()],
        }
    }

    #[test]
    fn ruleset_intercepts_locally_generated_traffic() {
        let r = ruleset(&cfg());
        // Sidecar model: intercept in the nat output hook (agent traffic is
        // locally generated in the shared netns), not a forwarding router.
        assert!(r.contains("nat hook output"));
        assert!(!r.contains("hook forward"));
        assert!(!r.contains("masquerade"));
    }

    #[test]
    fn ruleset_exempts_the_gateway_uid() {
        let r = ruleset(&cfg());
        // The gateway's own egress (proxy upstream, DNS forward, control) must not
        // be redirected back into the proxy.
        assert!(r.contains("meta skuid 0 return"));
        assert!(r.contains("ip daddr 127.0.0.0/8 return"));
    }

    #[test]
    fn ruleset_redirects_all_tcp_and_dns() {
        let r = ruleset(&cfg());
        // DNS to the resolver, and a catch-all that sends every TCP port to the
        // single sniffing proxy (hostname precision on any port).
        assert!(r.contains("udp dport 53 redirect to :53"));
        assert!(r.contains("tcp dport 53 redirect to :53"));
        assert!(r.contains("meta l4proto tcp redirect to :8443"));
    }

    #[test]
    fn ruleset_filter_drops_non_tcp_residue() {
        let r = ruleset(&cfg());
        // filter output drops by default; the REDIRECT can't carry UDP/ICMP, so the
        // filter chain is what stops non-DNS UDP and ICMP from leaking.
        assert!(r.contains("filter hook output"));
        assert!(r.contains("policy drop"));
        assert!(r.contains("udp dport 53 accept"));
        assert!(r.contains("ct state established,related accept"));
    }

    #[test]
    fn ruleset_exempts_approved_subnets_both_chains() {
        let r = ruleset(&cfg());
        // Approved Compose networks bypass the proxy (nat return) and are allowed
        // out (filter accept).
        assert!(r.contains("ip daddr 172.20.0.0/16 return"));
        assert!(r.contains("ip daddr 172.20.0.0/16 accept"));
    }
}
