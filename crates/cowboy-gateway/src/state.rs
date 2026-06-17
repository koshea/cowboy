//! Gateway decision state: ties together the policy, the DNS map, the scope
//! cache, and the host control client into a single `decide` entry point.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use cowboy_core::netproto::{NetworkAttempt, Protocol, Verdict};
use cowboy_core::policy;

use crate::control::ControlClient;
use crate::dns::DnsMap;
use crate::dns_policy::{self, RateTracker};

/// Shared gateway state.
pub struct GatewayState {
    policy: cowboy_core::config::NetworkPolicy,
    dns: DnsMap,
    control: ControlClient,
    /// Cache of approved/denied destinations from prior `ask` decisions.
    cache: Mutex<HashMap<String, Verdict>>,
    /// Per-parent DNS query-rate tracker (tunnel signal).
    dns_rate: RateTracker,
}

impl GatewayState {
    pub fn new(
        policy: cowboy_core::config::NetworkPolicy,
        dns: DnsMap,
        control: ControlClient,
    ) -> Self {
        let dns_rate = RateTracker::new(policy.dns.max_subdomains_per_min);
        Self {
            policy,
            dns,
            control,
            cache: Mutex::new(HashMap::new()),
            dns_rate,
        }
    }

    /// Access the DNS map (used by the resolver task and tests).
    pub fn dns(&self) -> &DnsMap {
        &self.dns
    }

    /// Enrich an attempt with a hostname from the DNS map if we only have an IP.
    pub fn enrich(&self, mut attempt: NetworkAttempt) -> NetworkAttempt {
        if attempt.host.is_none() {
            if let Some(ip) = attempt.ip {
                if let Some(host) = self.dns.lookup(ip) {
                    attempt.host = Some(host);
                }
            }
        }
        attempt
    }

    fn cache_key(attempt: &NetworkAttempt) -> String {
        // Key by host (not host:port) so one approval covers a host across the DNS
        // resolution (port 53) and the subsequent connection (443/80) — no double
        // prompt. IP-only attempts still key by ip:port.
        match &attempt.host {
            Some(h) => format!("host:{}", h.trim_end_matches('.').to_ascii_lowercase()),
            None => match attempt.ip {
                Some(ip) => format!("ip:{ip}:{}", attempt.port),
                None => format!("port:{}", attempt.port),
            },
        }
    }

    /// Decide the verdict for an attempt: policy first, then a cached `ask`
    /// result, then the host (which may persist a new scope).
    pub async fn decide(&self, attempt: &NetworkAttempt) -> Verdict {
        let (verdict, reason) = policy::evaluate(&self.policy, attempt);
        self.run_decision(attempt, verdict, reason).await
    }

    /// Apply a precomputed (verdict, reason) for an attempt: log allow/deny as an
    /// event, or run the cached/`ask` flow with the host. Shared by `decide`
    /// (connect layer) and `decide_dns` (resolver). `reason` is also shown in the
    /// host's `ask` prompt.
    pub(crate) async fn run_decision(
        &self,
        attempt: &NetworkAttempt,
        verdict: Verdict,
        reason: String,
    ) -> Verdict {
        match verdict {
            Verdict::Allow | Verdict::Deny => {
                self.control.event(attempt, verdict, reason).await;
                verdict
            }
            Verdict::Ask => {
                let key = Self::cache_key(attempt);
                if let Some(cached) = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&key)
                    .copied()
                {
                    return cached;
                }
                let decided = self.control.ask(attempt, Some(&reason)).await;
                // Cache concrete decisions (not a re-ask).
                if decided != Verdict::Ask {
                    self.cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key, decided);
                }
                decided
            }
        }
    }

    /// Record a DNS resolution (used by the resolver task).
    #[allow(dead_code)]
    pub fn record_dns(&self, ip: IpAddr, host: String) {
        self.dns.record(ip, host);
    }

    /// Decide whether to resolve a DNS query for `qname`/`qtype`.
    ///
    /// Crucially, **resolution never blocks on a human**: a DNS resolver gives up
    /// in seconds, so parking a query on an interactive `ask` just times it out
    /// (and makes clients hot-retry). Resolution also isn't egress — the actual
    /// connection is gated at connect-time, where prompting is safe and the
    /// verdict is cached per-host. So a name that would be `ask` is *resolved*
    /// here and approved (or denied) when it's connected to.
    ///
    /// The DNS layer therefore only ever **refuses** (fast, fail-closed) or
    /// **allows** (resolve): disallowed record types, deny-listed names, and
    /// suspected tunnels are refused — a tunnel's payload *is* the query, so there
    /// is no later connection to gate — and everything else resolves.
    pub async fn decide_dns(&self, qname: &str, qtype: &str) -> Verdict {
        let dns = &self.policy.dns;
        let attempt = NetworkAttempt {
            protocol: Protocol::Dns,
            host: Some(qname.trim_end_matches('.').to_string()),
            ip: None,
            port: 53,
        };

        // 1. Record-type gate (TXT/NULL/ANY/… carry tunnels/C2).
        if !dns_policy::qtype_allowed(qtype, dns) {
            return self
                .run_decision(
                    &attempt,
                    Verdict::Deny,
                    format!("dns: record type {qtype} not allowed"),
                )
                .await;
        }

        // 2. Name verdict.
        let (verdict, mut reason) = if dns.enforce {
            policy::evaluate_name(&self.policy, qname)
        } else {
            // Permissive: only the deny-list gates resolution; otherwise allow.
            match policy::evaluate_name(&self.policy, qname) {
                (Verdict::Deny, r) => (Verdict::Deny, r),
                (_, r) => (Verdict::Allow, r),
            }
        };

        // 3. Tunnel detection refuses a non-deny verdict (no connection follows a
        //    DNS-tunnel query, so this is the only place to stop it).
        let mut verdict = verdict;
        if dns.tunnel_detection && verdict != Verdict::Deny {
            let why = dns_policy::tunnel_reason(qname, dns).or_else(|| {
                let parent = dns_policy::registrable_parent(qname);
                self.dns_rate
                    .over_limit(&parent)
                    .then(|| format!("high query rate for {parent}"))
            });
            if let Some(why) = why {
                verdict = Verdict::Deny;
                reason = format!("dns tunnel suspected: {why}");
            }
        }

        // Refuse denials (logged + surfaced); resolve everything else. An `ask`
        // name resolves quietly — the connection layer prompts and caches it, so
        // the resolver is never blocked.
        match verdict {
            Verdict::Deny => self.run_decision(&attempt, Verdict::Deny, reason).await,
            _ => Verdict::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::{DefaultVerdict, NetworkPolicy};
    use cowboy_core::netproto::Protocol;

    fn state(policy: NetworkPolicy) -> GatewayState {
        GatewayState::new(policy, DnsMap::new(), ControlClient::new(None, None))
    }

    fn attempt(host: Option<&str>, ip: Option<&str>, port: u16) -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: host.map(String::from),
            ip: ip.map(|s| s.parse().unwrap()),
            port,
        }
    }

    #[tokio::test]
    async fn allow_listed_domain_allows() {
        let mut p = NetworkPolicy::default();
        p.allow.domains.push("github.com".into());
        p.allow.ports = vec![443];
        let s = state(p);
        assert_eq!(
            s.decide(&attempt(Some("github.com"), None, 443)).await,
            Verdict::Allow
        );
    }

    #[tokio::test]
    async fn metadata_denied() {
        let s = state(NetworkPolicy::default());
        assert_eq!(
            s.decide(&attempt(None, Some("169.254.169.254"), 80)).await,
            Verdict::Deny
        );
    }

    #[tokio::test]
    async fn ask_without_control_socket_fails_closed() {
        // default_external = ask, no control socket -> deny.
        let s = state(NetworkPolicy::default());
        assert_eq!(
            s.decide(&attempt(Some("unknown.test"), None, 443)).await,
            Verdict::Deny
        );
    }

    #[tokio::test]
    async fn allow_default_passes() {
        let p = NetworkPolicy {
            default_external: DefaultVerdict::Allow,
            ..Default::default()
        };
        let s = state(p);
        assert_eq!(
            s.decide(&attempt(Some("anything.test"), None, 443)).await,
            Verdict::Allow
        );
    }

    #[test]
    fn enrich_fills_host_from_dns_map() {
        let s = state(NetworkPolicy::default());
        s.dns()
            .record("1.2.3.4".parse().unwrap(), "host.test".into());
        let enriched = s.enrich(attempt(None, Some("1.2.3.4"), 443));
        assert_eq!(enriched.host.as_deref(), Some("host.test"));
    }

    // --- DNS policy ---

    #[tokio::test]
    async fn dns_allow_listed_name_resolves() {
        let mut p = NetworkPolicy::default();
        p.allow.domains.push("github.com".into());
        let s = state(p);
        // Allowed by name regardless of the allow port-list (resolution != connect).
        assert_eq!(s.decide_dns("api.github.com", "A").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn dns_unknown_name_resolves_and_is_gated_at_connect() {
        // default_external = ask: the name RESOLVES (resolution can't block on a
        // human); egress is gated at connect-time, not here.
        let s = state(NetworkPolicy::default());
        assert_eq!(s.decide_dns("evil.test", "A").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn dns_denied_name_refused() {
        let s = state(NetworkPolicy::default()); // denies metadata.google.internal
        assert_eq!(
            s.decide_dns("metadata.google.internal", "A").await,
            Verdict::Deny
        );
    }

    #[tokio::test]
    async fn dns_risky_qtype_refused_even_when_allowed() {
        let mut p = NetworkPolicy::default();
        p.allow.domains.push("github.com".into());
        let s = state(p);
        // A is fine; TXT is refused by the default qtype allowlist.
        assert_eq!(s.decide_dns("github.com", "A").await, Verdict::Allow);
        assert_eq!(s.decide_dns("github.com", "TXT").await, Verdict::Deny);
    }

    #[tokio::test]
    async fn dns_tunnel_on_allowed_parent_is_refused() {
        // Allow the parent, but a high-entropy/long label under it is suspicious →
        // refused outright (a DNS tunnel has no later connection to gate).
        let mut p = NetworkPolicy::default();
        p.allow.domains.push("evil.com".into());
        let s = state(p);
        let tunnel = "mfrggzdfmztwq2lknnwg23tpobyxe43uov3ho6dzpiztgmzr.evil.com";
        assert_eq!(s.decide_dns(tunnel, "A").await, Verdict::Deny);
        // A normal name under the same allowed parent still resolves.
        assert_eq!(s.decide_dns("api.evil.com", "A").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn dns_permissive_resolves_non_denied() {
        let mut p = NetworkPolicy {
            dns: cowboy_core::config::DnsPolicy {
                enforce: false,
                tunnel_detection: false,
                ..Default::default()
            },
            ..Default::default()
        };
        // metadata is denied; anything else resolves under permissive mode.
        p.allow.domains.clear();
        let s = state(p);
        assert_eq!(s.decide_dns("whatever.test", "A").await, Verdict::Allow);
        assert_eq!(
            s.decide_dns("metadata.google.internal", "A").await,
            Verdict::Deny
        );
    }
}
