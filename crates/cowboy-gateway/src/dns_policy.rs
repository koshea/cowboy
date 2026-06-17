//! DNS record-type gating + tunnel-detection heuristics.
//!
//! Pure logic (plus a small windowed rate tracker) so it's easy to test and can't
//! panic on adversarial names. The heuristics are deliberately conservative —
//! they require a strong signal (a very long/encoded subdomain region, or a high
//! per-domain query rate) so legitimate hashed hostnames (CDN names, etc.) don't
//! trip them. A hit doesn't hard-block; it escalates the verdict to `ask`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cowboy_core::config::DnsPolicy;

/// Is this record type allowed to resolve under the policy? Case-insensitive.
pub fn qtype_allowed(qtype: &str, policy: &DnsPolicy) -> bool {
    policy
        .allowed_qtypes
        .iter()
        .any(|a| a.eq_ignore_ascii_case(qtype))
}

/// The registrable-ish parent of a name (v1: the last two labels). Used to group
/// query-rate signals so all `<sub>.evil.com` lookups count against `evil.com`.
pub fn registrable_parent(name: &str) -> String {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
    let n = labels.len();
    if n >= 2 {
        format!("{}.{}", labels[n - 2], labels[n - 1])
    } else {
        name
    }
}

/// If `qname` looks like a DNS tunnel, return a short human reason; else `None`.
/// Looks at name/label length and the entropy of the subdomain region (the part
/// below the registrable parent), which is where tunneled data is encoded.
pub fn tunnel_reason(qname: &str, policy: &DnsPolicy) -> Option<String> {
    let name = qname.trim_end_matches('.');
    if name.is_empty() {
        return None;
    }
    if name.len() > policy.max_qname_len as usize {
        return Some(format!("query name unusually long ({} bytes)", name.len()));
    }
    let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
    let n = labels.len();
    // Subdomain region = everything below the last two labels (the parent).
    let sub = if n > 2 { &labels[..n - 2] } else { &[][..] };

    for lbl in sub {
        if lbl.len() > policy.max_label_len as usize {
            return Some(format!("label unusually long ({} chars)", lbl.len()));
        }
    }

    // Chunked encoding: tunnels often split data across many short labels. Normal
    // names rarely exceed a few subdomain levels.
    if sub.len() >= 6 {
        return Some(format!("{} subdomain levels", sub.len()));
    }

    // Encoded-data fingerprint: a long subdomain region with high Shannon entropy
    // (random hex ~3.8–4.0, base32 up to ~5 bits/char). Require BOTH a long region
    // and high entropy. The action is `ask`, so an occasional false positive only
    // costs one prompt — we'd rather catch borderline cases.
    let joined: String = sub.concat();
    if joined.len() >= 25 {
        let bits = shannon_entropy(&joined);
        if bits >= 3.5 {
            return Some(format!(
                "high-entropy subdomain ({} chars, {bits:.1} bits/char)",
                joined.len()
            ));
        }
    }
    None
}

/// Shannon entropy of `s` in bits per character (0 for empty).
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let len = s.chars().count() as f64;
    -counts
        .values()
        .map(|&c| {
            let p = c as f64 / len;
            p * p.log2()
        })
        .sum::<f64>()
}

/// Sliding 1-minute per-parent query-rate tracker. Many distinct lookups under one
/// parent in a short window is the strongest tunnel signal.
pub struct RateTracker {
    inner: Mutex<HashMap<String, Window>>,
    max_per_min: u32,
}

struct Window {
    count: u32,
    start: Instant,
}

impl RateTracker {
    pub fn new(max_per_min: u32) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_per_min,
        }
    }

    /// Record a query for `parent` (now) and report whether it's over the
    /// per-minute limit. Uses a fixed 60s tumbling window per parent.
    pub fn over_limit(&self, parent: &str) -> bool {
        self.over_limit_at(parent, Instant::now())
    }

    fn over_limit_at(&self, parent: &str, now: Instant) -> bool {
        if self.max_per_min == 0 {
            return false; // disabled
        }
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let w = map.entry(parent.to_string()).or_insert(Window {
            count: 0,
            start: now,
        });
        if now.duration_since(w.start) >= Duration::from_secs(60) {
            w.count = 0;
            w.start = now;
        }
        w.count += 1;
        w.count > self.max_per_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn policy() -> DnsPolicy {
        DnsPolicy::default()
    }

    #[test]
    fn qtype_gate_uses_allowlist() {
        let p = policy();
        assert!(qtype_allowed("A", &p));
        assert!(qtype_allowed("aaaa", &p)); // case-insensitive
        assert!(qtype_allowed("CNAME", &p));
        assert!(!qtype_allowed("TXT", &p));
        assert!(!qtype_allowed("ANY", &p));
        assert!(!qtype_allowed("AXFR", &p));
    }

    #[test]
    fn registrable_parent_takes_last_two_labels() {
        assert_eq!(registrable_parent("a.b.evil.com."), "evil.com");
        assert_eq!(registrable_parent("github.com"), "github.com");
        assert_eq!(registrable_parent("localhost"), "localhost");
    }

    #[test]
    fn benign_names_are_not_flagged() {
        let p = policy();
        for name in [
            "github.com",
            "api.github.com",
            "registry.npmjs.org",
            "d111111abcdef8.cloudfront.net", // typical CDN hash label
            "objects.githubusercontent.com",
            "k8s.gcr.io",
        ] {
            assert_eq!(tunnel_reason(name, &p), None, "{name} should be clean");
        }
    }

    #[test]
    fn tunnel_shaped_names_are_flagged() {
        let p = policy();
        // A long base32-ish encoded label.
        let label = "mfrggzdfmztwq2lknnwg23tpobyxe43uov3ho6dzpiztgmzr"; // 47 chars
        assert!(tunnel_reason(&format!("{label}.evil.com"), &p).is_some());
        // A multi-label high-entropy region.
        let n = "7a3f.9c2e.b8d1.4f6a.0e5c.aa11.bb22.cc33.dd44.exfil.net";
        assert!(tunnel_reason(n, &p).is_some());
        // An absurdly long overall name.
        let long = format!("{}.example.com", "a1b2c3d4.".repeat(25));
        assert!(tunnel_reason(&long, &p).is_some());
    }

    #[test]
    fn rate_tracker_trips_over_limit_and_resets() {
        let rt = RateTracker::new(3);
        let t0 = Instant::now();
        assert!(!rt.over_limit_at("evil.com", t0)); // 1
        assert!(!rt.over_limit_at("evil.com", t0)); // 2
        assert!(!rt.over_limit_at("evil.com", t0)); // 3
        assert!(rt.over_limit_at("evil.com", t0)); // 4 > 3
                                                   // A different parent has its own window.
        assert!(!rt.over_limit_at("github.com", t0));
        // After the window elapses, it resets.
        let later = t0 + Duration::from_secs(61);
        assert!(!rt.over_limit_at("evil.com", later));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn tunnel_reason_never_panics(s in ".{0,400}") {
            let _ = tunnel_reason(&s, &policy());
            let _ = registrable_parent(&s);
        }
    }
}
