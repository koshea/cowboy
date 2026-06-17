//! nftables ruleset for the gateway container.
//!
//! This is the load-bearing enforcement: the `forward` chain DROPs by default,
//! so the gateway is **not** an open router. Only TCP 80/443 from the agent
//! subnet are REDIRECTed to the in-process proxy (which applies policy); DNS is
//! accepted to the gateway's own resolver; everything else outbound is dropped.
//! Applying the ruleset is fatal on failure — we fail closed, never open.

use anyhow::{bail, Context, Result};

use crate::config::{GatewayConfig, PORT_HTTP, PORT_TLS};

/// Render the nft ruleset for the given config.
pub fn ruleset(cfg: &GatewayConfig) -> String {
    let subnet = &cfg.agent_subnet;
    let mut allow_rules = String::new();
    for net in &cfg.allow_subnets {
        // Approved Docker/Compose networks: allow forwarded traffic to them.
        allow_rules.push_str(&format!("    ip daddr {net} accept\n"));
    }

    format!(
        "table ip cowboy {{
  chain prerouting {{
    type nat hook prerouting priority dstnat; policy accept;
    ip saddr {subnet} tcp dport 443 redirect to :{PORT_TLS}
    ip saddr {subnet} tcp dport 80 redirect to :{PORT_HTTP}
  }}
  chain forward {{
    type filter hook forward priority filter; policy drop;
    ct state established,related accept
{allow_rules}  }}
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
    fn ruleset_forward_drops_by_default() {
        let r = ruleset(&cfg());
        assert!(r.contains("chain forward"));
        assert!(r.contains("policy drop"));
        assert!(r.contains("ct state established,related accept"));
    }

    #[test]
    fn ruleset_redirects_only_80_and_443_from_agent_subnet() {
        let r = ruleset(&cfg());
        assert!(r.contains("ip saddr 10.88.0.0/24 tcp dport 443 redirect to :8443"));
        assert!(r.contains("ip saddr 10.88.0.0/24 tcp dport 80 redirect to :8081"));
        // No blanket masquerade/accept that would turn it into an open router.
        assert!(!r.contains("masquerade"));
    }

    #[test]
    fn ruleset_allows_approved_subnets() {
        let r = ruleset(&cfg());
        assert!(r.contains("ip daddr 172.20.0.0/16 accept"));
    }
}
