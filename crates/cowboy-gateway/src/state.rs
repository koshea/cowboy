//! Gateway decision state: ties together the policy, the DNS map, the scope
//! cache, and the host control client into a single `decide` entry point.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use cowboy_core::netproto::{NetworkAttempt, Verdict};
use cowboy_core::policy;

use crate::control::ControlClient;
use crate::dns::DnsMap;

/// Shared gateway state.
pub struct GatewayState {
    policy: cowboy_core::config::NetworkPolicy,
    dns: DnsMap,
    control: ControlClient,
    /// Cache of approved/denied destinations from prior `ask` decisions.
    cache: Mutex<HashMap<String, Verdict>>,
}

impl GatewayState {
    pub fn new(
        policy: cowboy_core::config::NetworkPolicy,
        dns: DnsMap,
        control: ControlClient,
    ) -> Self {
        Self {
            policy,
            dns,
            control,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Access the DNS map (used by the resolver task and tests).
    #[allow(dead_code)]
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
        match &attempt.host {
            Some(h) => format!("host:{h}:{}", attempt.port),
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
        match verdict {
            Verdict::Allow | Verdict::Deny => {
                self.control.event(attempt, verdict, reason).await;
                verdict
            }
            Verdict::Ask => {
                let key = Self::cache_key(attempt);
                if let Some(cached) = self.cache.lock().unwrap().get(&key).copied() {
                    return cached;
                }
                let decided = self.control.ask(attempt).await;
                // Cache concrete decisions (not a re-ask).
                if decided != Verdict::Ask {
                    self.cache.lock().unwrap().insert(key, decided);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::{DefaultVerdict, NetworkPolicy};
    use cowboy_core::netproto::Protocol;

    fn state(policy: NetworkPolicy) -> GatewayState {
        GatewayState::new(policy, DnsMap::new(), ControlClient::new(None))
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
}
