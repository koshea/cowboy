//! nftables ruleset for the gateway, running as a **sidecar in the agent's
//! network namespace**.
//!
//! The gateway shares the agent's netns, so the agent's outbound traffic is
//! *locally generated* and we intercept it in two hooks:
//!
//! - `nat output` REDIRECTs **all** of the agent's TCP to the in-process proxy
//!   (DNS to the resolver), which applies policy. The proxy sniffs each connection
//!   (SNI/Host on any port) and falls back to the DNS map for opaque traffic.
//!   The DNS redirect sits **above** the loopback `return` and the chain runs at
//!   priority `-150` (ahead of Docker's `dns-dnat` at `-100`), so even the agent's
//!   queries to Docker's embedded resolver (`127.0.0.11:53`) are caught and
//!   gated/recorded here first; the resolver then forwards them on to `127.0.0.11`.
//! - `filter output` then **drops by default**, so the residue the REDIRECT can't
//!   carry — non-DNS UDP, ICMP — can't leak. This is what restores deny-by-default
//!   on every port/protocol.
//!
//! The gateway's own egress (the proxy's upstream dials, DNS forwarding, the host
//! control channel) — and Docker's embedded DNS resolver, which also runs as root
//! — are exempted by `skuid 0`. The agent must therefore never run as uid 0; the
//! host remaps it to a non-root uid when `cowboy` itself runs as root (see
//! `runtime::host_user`), so a root operator can't make the agent inherit this
//! exemption and bypass the proxy. Approved Compose subnets bypass the proxy and
//! are accepted directly.
//!
//! IPv6 is dropped wholesale (`table ip6`): the proxy is IPv4-only, so a separate
//! default-drop chain makes v6 fail **closed** independent of the `disable_ipv6`
//! sysctl the host also sets. Applying the ruleset is fatal on failure — we fail
//! closed, never open.

use anyhow::{bail, Context, Result};

use crate::config::{GatewayConfig, PORT_DNS, PORT_TLS};

/// The uid whose egress is exempt: root, covering the gateway process and Docker's
/// embedded DNS resolver. The agent is kept non-root so it never matches.
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
    type nat hook output priority -150; policy accept;
    meta skuid {GATEWAY_UID} return
    udp dport 53 redirect to :{PORT_DNS}
    tcp dport 53 redirect to :{PORT_DNS}
    ip daddr 127.0.0.0/8 return
{nat_exempt}    meta l4proto tcp redirect to :{PORT_TLS}
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
table ip6 cowboy {{
  chain filter_out {{
    type filter hook output priority 0; policy drop;
    meta skuid {GATEWAY_UID} accept
    ip6 daddr ::1 accept
    ct state established,related accept
  }}
}}
"
    )
}

/// Apply the ruleset via `nft -f -`. Fatal on failure (fail-closed).
pub async fn apply(cfg: &GatewayConfig) -> Result<()> {
    let rules = ruleset(cfg);
    // Flush any prior cowboy tables (create-if-missing then delete, so the first
    // run doesn't error), then load.
    let script = format!(
        "table ip cowboy\ndelete table ip cowboy\n\
         table ip6 cowboy\ndelete table ip6 cowboy\n{rules}"
    );
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
    fn ruleset_exempts_root_egress() {
        let r = ruleset(&cfg());
        // The gateway's own egress (proxy upstream, DNS forward, control) and
        // Docker's embedded DNS resolver both run as root and are exempted by
        // `skuid 0`. The agent is kept non-root (see runtime::host_user) so it
        // never matches this exemption.
        assert!(r.contains("meta skuid 0 return"));
        assert!(r.contains("meta skuid 0 accept"));
        assert!(r.contains("ip daddr 127.0.0.0/8 return"));
    }

    #[test]
    fn ruleset_drops_ipv6_by_default() {
        let r = ruleset(&cfg());
        // The proxy is IPv4-only; v6 must fail closed regardless of the sysctl.
        assert!(r.contains("table ip6 cowboy"));
        // The v6 chain default-drops and only lets the gateway / loopback /
        // established traffic through.
        let v6 = r.split("table ip6 cowboy").nth(1).unwrap();
        assert!(v6.contains("policy drop"));
        assert!(v6.contains("ip6 daddr ::1 accept"));
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
