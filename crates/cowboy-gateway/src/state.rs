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

    /// Decide a **transparent** connection, authorizing it by the hostname(s)
    /// *this gateway resolved* for the dialed IP — never the client-presented
    /// SNI/Host, which a malicious agent controls (we don't MITM). Precedence:
    /// any resolved name that the policy denies wins; else any that it allows;
    /// else the IP-class default. With no resolved name (a raw-IP connection) it
    /// falls back to IP-only matching (CIDR allow/deny + class default). Returns
    /// the verdict and the attempt actually used (for the splice/logging).
    pub async fn decide_connection(
        &self,
        ip: IpAddr,
        port: u16,
        protocol: Protocol,
    ) -> (Verdict, NetworkAttempt) {
        let names = self.dns.lookup_all(ip);
        let mk = |host: Option<String>| NetworkAttempt {
            protocol,
            host,
            ip: Some(ip),
            port,
        };

        // No resolved name: decide by IP alone (CIDR/classify).
        if names.is_empty() {
            let a = mk(None);
            let (v, r) = policy::evaluate(&self.policy, &a);
            let v = self.run_decision(&a, v, r).await;
            return (v, a);
        }

        // Evaluate every resolved name; combine by policy precedence.
        let evals: Vec<(NetworkAttempt, Verdict, String)> = names
            .into_iter()
            .map(|n| {
                let a = mk(Some(n));
                let (v, r) = policy::evaluate(&self.policy, &a);
                (a, v, r)
            })
            .collect();

        // Deny wins over any allow.
        if let Some((a, _, r)) = evals.iter().find(|(_, v, _)| *v == Verdict::Deny) {
            let v = self.run_decision(a, Verdict::Deny, r.clone()).await;
            return (v, a.clone());
        }
        // Then any allow.
        if let Some((a, _, r)) = evals.iter().find(|(_, v, _)| *v == Verdict::Allow) {
            let v = self.run_decision(a, Verdict::Allow, r.clone()).await;
            return (v, a.clone());
        }
        // Otherwise all names ask → run the ask flow on the first (representative).
        let (a, v, r) = &evals[0];
        let v = self.run_decision(a, *v, r.clone()).await;
        (v, a.clone())
    }

    fn cache_key(attempt: &NetworkAttempt) -> String {
        // Key by host+port so an approval is scoped to the port it was granted for
        // (approving `evil.com:443` must not also open `evil.com:22`). DNS
        // resolution is keyed separately (port 53) and gated by name, not cached
        // here, so this doesn't reintroduce a double prompt for the 53→443 step.
        match &attempt.host {
            Some(h) => format!(
                "host:{}:{}",
                h.trim_end_matches('.').to_ascii_lowercase(),
                attempt.port
            ),
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
        //    DNS-tunnel query, so this is the only place to stop it). Two checks:
        //    - SHAPE (entropy/length/chunking) applies to any non-denied name — it
        //      catches `<exfil>.allowed.com` even under an allowed parent.
        //    - the query-RATE limiter applies only to UNKNOWN (`Ask`) names: a burst
        //      of lookups to an explicitly ALLOWED domain (e.g. `bundle install`
        //      hammering `index.rubygems.org`, or npm/cargo) is legitimate, not a
        //      tunnel — rate-limiting it silently breaks the allow-list.
        let mut verdict = verdict;
        if dns.tunnel_detection && verdict != Verdict::Deny {
            let why = dns_policy::tunnel_reason(qname, dns).or_else(|| {
                if verdict != Verdict::Ask {
                    return None;
                }
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
    async fn high_query_rate_to_allowed_domain_is_not_rate_denied() {
        // `bundle install` / npm / cargo hammer an allowed package index's host
        // with far more than the per-minute limit of lookups — that's legitimate,
        // so the rate limiter must NOT deny an explicitly allowed domain (the
        // allow-list is the user's explicit trust). Regression for rubygems.org.
        let mut p = NetworkPolicy::default();
        p.allow.domains.push("rubygems.org".into());
        let s = state(p);
        for _ in 0..80 {
            assert_eq!(
                s.decide_dns("index.rubygems.org", "A").await,
                Verdict::Allow,
                "an allowed domain must never be rate-denied"
            );
        }

        // …but a burst to an UNKNOWN parent is still rate-limited (tunnel guard).
        let mut p2 = NetworkPolicy::default();
        p2.allow.domains.clear();
        let s2 = state(p2);
        let mut denied = false;
        for i in 0..80 {
            if s2
                .decide_dns(&format!("h{i}.unknown-parent.test"), "A")
                .await
                == Verdict::Deny
            {
                denied = true;
                break;
            }
        }
        assert!(
            denied,
            "a high query rate to an unknown parent should be denied"
        );
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
