//! Network policy evaluation.
//!
//! Pure decision logic shared by the gateway and host: given a [`NetworkPolicy`]
//! and an observed [`NetworkAttempt`], decide allow/deny/ask. This is the
//! security core, so it is exhaustively unit- and property-tested.
//!
//! Precedence (deny wins, then allow, then the default):
//! 1. An explicit **deny** match (domain or CIDR) → [`Verdict::Deny`].
//! 2. An explicit **allow** match (domain or CIDR), with the port allowed if a
//!    port allow-list is configured → [`Verdict::Allow`].
//! 3. Otherwise the `default_external` verdict.

use std::net::IpAddr;

use ipnet::IpNet;

use crate::config::{DefaultVerdict, NetworkPolicy, RuleSet};
use crate::netproto::{NetworkAttempt, Verdict};

impl From<DefaultVerdict> for Verdict {
    fn from(d: DefaultVerdict) -> Self {
        match d {
            DefaultVerdict::Allow => Verdict::Allow,
            DefaultVerdict::Deny => Verdict::Deny,
            DefaultVerdict::Ask => Verdict::Ask,
        }
    }
}

/// Evaluate an attempt against the policy, returning a verdict and a short
/// human-readable reason.
pub fn evaluate(policy: &NetworkPolicy, attempt: &NetworkAttempt) -> (Verdict, String) {
    // 1. Deny list — highest precedence.
    if let Some(reason) = matches_ruleset(&policy.deny, attempt, /* require_port */ false) {
        return (Verdict::Deny, format!("denied by policy ({reason})"));
    }

    // 2. Allow list. If the allow set lists ports, the port must be allowed too.
    if let Some(reason) = matches_ruleset(&policy.allow, attempt, /* require_port */ true) {
        return (Verdict::Allow, format!("allowed by policy ({reason})"));
    }

    // 3. Default for external destinations.
    (
        policy.default_external.into(),
        format!("default_external = {:?}", policy.default_external),
    )
}

/// Returns `Some(reason)` if the attempt matches the rule set. When
/// `require_port` is set and the rule set declares ports, the attempt's port
/// must be in the list for a match.
fn matches_ruleset(
    rules: &RuleSet,
    attempt: &NetworkAttempt,
    require_port: bool,
) -> Option<String> {
    let port_ok = !require_port || rules.ports.is_empty() || rules.ports.contains(&attempt.port);

    if let Some(host) = &attempt.host {
        if let Some(rule) = rules.domains.iter().find(|d| domain_matches(d, host)) {
            if port_ok {
                return Some(format!("domain {rule}"));
            }
        }
    }

    if let Some(ip) = attempt.ip {
        if let Some(rule) = rules.cidrs.iter().find(|c| cidr_matches(c, ip)) {
            if port_ok {
                return Some(format!("cidr {rule}"));
            }
        }
    }

    None
}

/// A domain rule matches the host exactly, or as a parent (`github.com` matches
/// `api.github.com`). Matching is case-insensitive and ignores a trailing dot.
///
/// ```
/// use cowboy_core::policy::domain_matches;
/// assert!(domain_matches("github.com", "api.github.com"));
/// assert!(domain_matches("github.com", "GitHub.com"));
/// assert!(!domain_matches("github.com", "notgithub.com"));
/// assert!(!domain_matches("github.com", "github.com.evil.com"));
/// ```
pub fn domain_matches(rule: &str, host: &str) -> bool {
    let rule = rule.trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if rule.is_empty() {
        return false;
    }
    host == rule || host.ends_with(&format!(".{rule}"))
}

/// True if `ip` falls within the CIDR (or equals the bare IP) given by `rule`.
pub fn cidr_matches(rule: &str, ip: IpAddr) -> bool {
    if let Ok(net) = rule.parse::<IpNet>() {
        return net.contains(&ip);
    }
    // Allow a bare IP literal as a /32 or /128.
    if let Ok(single) = rule.parse::<IpAddr>() {
        return single == ip;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DefaultVerdict;
    use crate::netproto::Protocol;

    fn attempt(host: Option<&str>, ip: Option<&str>, port: u16) -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: host.map(String::from),
            ip: ip.map(|s| s.parse().unwrap()),
            port,
        }
    }

    #[test]
    fn default_policy_denies_metadata_ip() {
        let policy = NetworkPolicy::default();
        let (v, _) = evaluate(&policy, &attempt(None, Some("169.254.169.254"), 80));
        assert_eq!(v, Verdict::Deny);
    }

    #[test]
    fn default_policy_asks_unknown_external() {
        let policy = NetworkPolicy::default();
        let (v, _) = evaluate(&policy, &attempt(Some("example.com"), None, 443));
        assert_eq!(v, Verdict::Ask);
    }

    #[test]
    fn allowed_domain_allows_and_subdomains_match() {
        let mut policy = NetworkPolicy::default();
        policy.allow.domains.push("github.com".into());
        policy.allow.ports = vec![443];
        assert_eq!(
            evaluate(&policy, &attempt(Some("github.com"), None, 443)).0,
            Verdict::Allow
        );
        assert_eq!(
            evaluate(&policy, &attempt(Some("api.github.com"), None, 443)).0,
            Verdict::Allow
        );
    }

    #[test]
    fn allow_respects_port_restriction() {
        let mut policy = NetworkPolicy::default();
        policy.allow.domains.push("github.com".into());
        policy.allow.ports = vec![443];
        // Port 22 is not in the allow ports -> falls through to default (ask).
        assert_eq!(
            evaluate(&policy, &attempt(Some("github.com"), None, 22)).0,
            Verdict::Ask
        );
    }

    #[test]
    fn deny_beats_allow() {
        let mut policy = NetworkPolicy::default();
        policy.allow.domains.push("evil.example".into());
        policy.deny.domains.push("evil.example".into());
        assert_eq!(
            evaluate(&policy, &attempt(Some("evil.example"), None, 443)).0,
            Verdict::Deny
        );
    }

    #[test]
    fn domain_matching_is_not_fooled_by_suffix_tricks() {
        // notgithub.com must NOT match github.com.
        assert!(!domain_matches("github.com", "notgithub.com"));
        assert!(domain_matches("github.com", "GitHub.com"));
        assert!(domain_matches("github.com", "a.b.github.com"));
        assert!(!domain_matches("github.com", "github.com.evil.com"));
    }

    #[test]
    fn cidr_matching() {
        assert!(cidr_matches(
            "169.254.169.254/32",
            "169.254.169.254".parse().unwrap()
        ));
        assert!(cidr_matches("10.0.0.0/8", "10.1.2.3".parse().unwrap()));
        assert!(!cidr_matches("10.0.0.0/8", "11.0.0.1".parse().unwrap()));
        assert!(cidr_matches("1.2.3.4", "1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn default_allow_verdict_passthrough() {
        let policy = NetworkPolicy {
            default_external: DefaultVerdict::Allow,
            ..Default::default()
        };
        // Unknown host with allow-default -> allow.
        assert_eq!(
            evaluate(&policy, &attempt(Some("whatever.test"), None, 443)).0,
            Verdict::Allow
        );
    }
}
