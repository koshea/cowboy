//! The nftables ruleset that makes the sandbox's traffic visible to the policy
//! engine.
//!
//! Rewritten for the host-native sandbox, not ported. The container-era ruleset
//! would have been actively **unsafe** here, for three reasons worth recording:
//!
//! 1. It exempted `meta skuid 0`, because the gateway ran as root in the shared
//!    network namespace while the agent was kept non-root. In this sandbox the agent
//!    *is* uid 0 in its user namespace, so that same rule would exempt the agent
//!    from interception entirely — an exact inversion of its purpose.
//! 2. It used `redirect`, which needs `nft_redir`. `dnat to 127.0.0.1:<port>` is
//!    equivalent here and needs only `nft_nat`, which the nat chain requires anyway.
//! 3. It ran at priority `-150` to get ahead of Docker's own `dns-dnat` chain, and
//!    exempted approved Compose subnets. Neither exists any more.
//!
//! The replacement is smaller because it has less to defend against: the sandbox
//! network namespace holds no host-connected device, so **containment does not
//! depend on this ruleset at all**. Its job is transparency — making the traffic
//! reach the relay instead of merely failing. A total failure to apply it means no
//! egress, not open egress.

use anyhow::{bail, Context, Result};

use super::TransportConfig;

/// Render the ruleset for a sandbox.
///
/// Two hooks:
///
/// - `nat output` rewrites every TCP destination to the relay, and DNS to the
///   resolver. It must run for the traffic to be *reachable*: a black-hole device
///   carries the route, and this is what catches the packet before it reaches
///   nowhere.
/// - `filter output` then drops by default, which is what stops the residue the
///   nat hook cannot carry — non-DNS UDP and ICMP.
pub fn ruleset(cfg: &TransportConfig) -> String {
    let relay = cfg.relay_port;
    let dns = cfg.dns_port;
    format!(
        "table ip cowboy {{
  chain out {{
    type nat hook output priority dstnat; policy accept;
    ip daddr 127.0.0.0/8 return
    udp dport 53 dnat to 127.0.0.1:{dns}
    tcp dport 53 dnat to 127.0.0.1:{dns}
    meta l4proto tcp dnat to 127.0.0.1:{relay}
  }}
  chain filt {{
    type filter hook output priority filter; policy drop;
    ip daddr 127.0.0.0/8 accept
    ip saddr 127.0.0.0/8 accept
  }}
}}
table ip6 cowboy {{
  chain filt {{
    type filter hook output priority filter; policy drop;
    ip6 daddr ::1 accept
    ip6 saddr ::1 accept
  }}
}}
"
    )
}

/// Apply the ruleset via `nft -f -`.
///
/// Must be called from inside the sandbox's network namespace while
/// `CAP_NET_ADMIN` is still held. Fatal on failure: the caller must not run a
/// command when this errors, because the traffic would be unroutable rather than
/// policed — safe, but silently broken.
pub async fn apply(cfg: &TransportConfig) -> Result<()> {
    let rules = ruleset(cfg);
    // Create-then-delete so a first run does not error on a missing table, then
    // load. Idempotent, so re-entering bring-up cannot accumulate duplicate rules.
    let script = format!(
        "table ip cowboy\ndelete table ip cowboy\n\
         table ip6 cowboy\ndelete table ip6 cowboy\n{rules}"
    );
    apply_script(&script)
        .await
        .context("applying the sandbox nft ruleset")
}

/// Whether the ruleset is currently loaded, for the transport's `verify`.
pub async fn is_loaded() -> bool {
    tokio::process::Command::new("nft")
        .args(["list", "table", "ip", "cowboy"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
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
        .context("spawning nft (is nftables installed?)")?;

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

    fn cfg() -> TransportConfig {
        TransportConfig {
            relay_port: 8443,
            dns_port: 5354,
            sandbox_cidr: "10.88.0.2/24".into(),
            blackhole_gateway: "10.88.0.1".into(),
        }
    }

    #[test]
    fn all_tcp_and_dns_are_redirected_to_the_relay() {
        let r = ruleset(&cfg());
        assert!(r.contains("meta l4proto tcp dnat to 127.0.0.1:8443"));
        assert!(r.contains("udp dport 53 dnat to 127.0.0.1:5354"));
        assert!(r.contains("tcp dport 53 dnat to 127.0.0.1:5354"));
    }

    /// `dnat` rather than `redirect`: equivalent here, but needs only `nft_nat`,
    /// which the nat chain requires anyway, instead of also `nft_redir`.
    #[test]
    fn uses_dnat_not_redirect() {
        let r = ruleset(&cfg());
        assert!(
            !r.contains("redirect"),
            "redirect adds an nft_redir dependency:\n{r}"
        );
    }

    /// The container-era rule exempted `skuid 0` because the gateway ran as root
    /// alongside a non-root agent. Here the agent *is* uid 0 in its user namespace,
    /// so that rule would exempt the agent from interception entirely.
    #[test]
    fn never_exempts_a_uid() {
        let r = ruleset(&cfg());
        assert!(
            !r.contains("skuid"),
            "a uid exemption would exempt the agent itself in this topology:\n{r}"
        );
    }

    /// The backstop must accept **both** directions. The relay's reply travels from
    /// 127.0.0.1 to the sandbox address and is locally generated output too, so a
    /// rule matching only `daddr` drops every SYN-ACK and the handshake times out —
    /// which presents as "interception silently does not work".
    #[test]
    fn the_backstop_accepts_both_loopback_directions() {
        let r = ruleset(&cfg());
        assert!(r.contains("ip daddr 127.0.0.0/8 accept"));
        assert!(
            r.contains("ip saddr 127.0.0.0/8 accept"),
            "without this the relay's SYN-ACK is dropped:\n{r}"
        );
    }

    /// `oifname "lo"` is not a substitute: the output interface is chosen before the
    /// nat hook rewrites the destination, so at the filter hook it is still the
    /// black-hole device.
    #[test]
    fn the_backstop_does_not_match_on_the_output_interface() {
        let r = ruleset(&cfg());
        assert!(
            !r.contains("oifname"),
            "the oif is decided pre-DNAT, so matching it does not work:\n{r}"
        );
    }

    #[test]
    fn non_dns_udp_and_icmp_are_dropped_by_default() {
        let r = ruleset(&cfg());
        let filt = r.split("chain filt").nth(1).unwrap();
        assert!(filt.contains("policy drop"));
        // No blanket TCP accept: redirected TCP already matches the loopback rule,
        // so accepting it explicitly would only help traffic that escaped the nat
        // hook — exactly what the backstop is for.
        assert!(!filt.contains("meta l4proto tcp accept"));
    }

    #[test]
    fn ipv6_fails_closed_independently() {
        let r = ruleset(&cfg());
        assert!(r.contains("table ip6 cowboy"));
        let v6 = r.split("table ip6 cowboy").nth(1).unwrap();
        assert!(v6.contains("policy drop"));
    }

    /// Re-running bring-up must not accumulate rules.
    #[test]
    fn the_apply_script_is_idempotent() {
        let rules = ruleset(&cfg());
        let script = format!(
            "table ip cowboy\ndelete table ip cowboy\n\
             table ip6 cowboy\ndelete table ip6 cowboy\n{rules}"
        );
        assert_eq!(script.matches("delete table ip cowboy").count(), 1);
        assert!(script.starts_with("table ip cowboy\ndelete table ip cowboy"));
    }
}
