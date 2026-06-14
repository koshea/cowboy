//! Property tests for the security-critical policy matcher.

use cowboy_core::config::{DefaultVerdict, NetworkPolicy};
use cowboy_core::netproto::{NetworkAttempt, Protocol, Verdict};
use cowboy_core::policy::{domain_matches, evaluate};
use proptest::prelude::*;

fn attempt(host: Option<String>, port: u16) -> NetworkAttempt {
    NetworkAttempt {
        protocol: Protocol::Tls,
        host,
        ip: None,
        port,
    }
}

proptest! {
    /// A domain rule must never match a host that does not end in `.rule` or
    /// equal `rule`. Guards against suffix-confusion bypasses.
    #[test]
    fn domain_match_requires_label_boundary(
        rule in "[a-z]{1,8}\\.[a-z]{2,4}",
        prefix in "[a-z0-9]{0,8}",
    ) {
        let host = format!("{prefix}{rule}"); // e.g. "evil" + "github.com"
        if prefix.is_empty() {
            prop_assert!(domain_matches(&rule, &host));
        } else {
            // Without a separating dot, it must not match.
            prop_assert!(!domain_matches(&rule, &host),
                "rule={rule} host={host} matched without a label boundary");
        }
    }

    /// If a host is in the deny list, the verdict is Deny regardless of the
    /// allow list or default. Deny always wins.
    #[test]
    fn deny_always_wins(
        host in "[a-z]{1,8}\\.[a-z]{2,4}",
        port in 1u16..=65535,
        default in prop::sample::select(vec![
            DefaultVerdict::Allow, DefaultVerdict::Deny, DefaultVerdict::Ask,
        ]),
    ) {
        let mut policy = NetworkPolicy { default_external: default, ..Default::default() };
        policy.allow.domains.push(host.clone()); // try to allow it
        policy.allow.ports.clear();
        policy.deny.domains.push(host.clone());   // but also deny it
        let (v, _) = evaluate(&policy, &attempt(Some(host), port));
        prop_assert_eq!(v, Verdict::Deny);
    }

    /// With an empty allow/deny set, every external attempt yields the default.
    #[test]
    fn empty_rules_yield_default(
        host in "[a-z]{1,8}\\.[a-z]{2,4}",
        port in 1u16..=65535,
    ) {
        let policy = NetworkPolicy {
            default_external: DefaultVerdict::Ask,
            allow: Default::default(),
            deny: Default::default(),
            ..Default::default()
        };
        let (v, _) = evaluate(&policy, &attempt(Some(host), port));
        prop_assert_eq!(v, Verdict::Ask);
    }
}
