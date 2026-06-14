//! Table-driven policy tests using rstest.

use cowboy_core::config::NetworkPolicy;
use cowboy_core::netproto::{NetworkAttempt, Protocol, Verdict};
use cowboy_core::policy::evaluate;
use rstest::rstest;

fn policy() -> NetworkPolicy {
    let mut p = NetworkPolicy::default();
    p.allow.domains.push("github.com".into());
    p.allow.cidrs.push("10.0.0.0/8".into());
    p.allow.ports = vec![443];
    p
}

fn attempt(host: Option<&str>, ip: Option<&str>, port: u16) -> NetworkAttempt {
    NetworkAttempt {
        protocol: Protocol::Tls,
        host: host.map(String::from),
        ip: ip.map(|s| s.parse().unwrap()),
        port,
    }
}

#[rstest]
#[case::allowed_domain(Some("github.com"), None, 443, Verdict::Allow)]
#[case::allowed_subdomain(Some("api.github.com"), None, 443, Verdict::Allow)]
#[case::allowed_domain_wrong_port(Some("github.com"), None, 22, Verdict::Ask)]
#[case::unlisted_domain(Some("example.com"), None, 443, Verdict::Ask)]
#[case::allowed_cidr(None, Some("10.1.2.3"), 443, Verdict::Allow)]
#[case::metadata_denied(None, Some("169.254.169.254"), 80, Verdict::Deny)]
#[case::suffix_trick_not_allowed(Some("notgithub.com"), None, 443, Verdict::Ask)]
fn policy_table(
    #[case] host: Option<&str>,
    #[case] ip: Option<&str>,
    #[case] port: u16,
    #[case] expected: Verdict,
) {
    let (verdict, _reason) = evaluate(&policy(), &attempt(host, ip, port));
    assert_eq!(verdict, expected);
}
