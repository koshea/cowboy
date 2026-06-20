//! Policy-enforcing forwarding DNS resolver.
//!
//! The agent can only resolve names through the gateway (its `--dns` points here
//! and direct egress to other resolvers is dropped). Every query is gated by the
//! policy *before* it leaves: only names the policy Allows or the user approves are
//! forwarded upstream; denied/unknown names, disallowed record types, and suspected
//! tunnels are answered REFUSED locally and never sent out. This closes DNS as an
//! exfiltration channel. Answers are still recorded as `ip -> domain` so the
//! transparent TCP path can map a connection's IP back to the requested hostname.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hickory_proto::op::{Message, ResponseCode};
use hickory_proto::rr::RData;
use tokio::net::UdpSocket;

use crate::state::GatewayState;
use cowboy_core::netproto::Verdict;

/// How long a resolved `IP → name` mapping is trusted for connect-time
/// attribution. The resolve→connect window is seconds; this is generous enough to
/// cover a session reusing an IP across a few connections, but short enough that a
/// reassigned/rebound IP doesn't stay authorized by a stale name for long.
const DNS_TTL: Duration = Duration::from_secs(600);
/// Cap names retained per IP (shared CDN IPs front many hosts) — bounds memory.
const MAX_NAMES_PER_IP: usize = 16;

/// Recently-resolved hostnames for one IP, each with the time it was recorded
/// (for TTL eviction).
type NameLog = Vec<(String, Instant)>;

/// Shared, thread-safe map of resolved IP -> the set of hostnames recently
/// resolved to it. A *set* (not one name) because CDN IPs front many hosts: a
/// connection is authorized if **any** recently-resolved name for its IP is
/// allowed, which avoids false denials when an allow-listed host shares a CDN IP
/// with others. Entries expire after [`DNS_TTL`].
#[derive(Debug, Clone, Default)]
pub struct DnsMap {
    inner: Arc<Mutex<HashMap<IpAddr, NameLog>>>,
}

impl DnsMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, ip: IpAddr, host: String) {
        let now = Instant::now();
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names = map.entry(ip).or_default();
        // Drop the prior copy of this name and any expired entries, then append.
        names.retain(|(h, t)| h != &host && now.duration_since(*t) < DNS_TTL);
        names.push((host, now));
        if names.len() > MAX_NAMES_PER_IP {
            let excess = names.len() - MAX_NAMES_PER_IP;
            names.drain(0..excess);
        }
    }

    /// All non-expired hostnames resolved to `ip` (oldest → newest).
    pub fn lookup_all(&self, ip: IpAddr) -> Vec<String> {
        let now = Instant::now();
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&ip)
            .map(|names| {
                names
                    .iter()
                    .filter(|(_, t)| now.duration_since(*t) < DNS_TTL)
                    .map(|(h, _)| h.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record every A/AAAA answer in a DNS response message, mapping each IP to
    /// the hostname(s) it should be attributed to.
    ///
    /// Crucially this records under the **queried name** (what the client asked
    /// for, and what allow-lists match), not only the A-record's *owner* — for a
    /// CNAME'd host (`files.pythonhosted.org → …fastly.net → 1.2.3.4`) the owner is
    /// the canonical CDN name, which no allow-list mentions. We also keep each
    /// record's owner so chain intermediates resolve too.
    pub fn record_answers(&self, msg: &Message) {
        let qname = msg.queries.first().map(|q| q.name().to_utf8());
        for record in &msg.answers {
            let ip = match &record.data {
                RData::A(a) => IpAddr::V4(a.0),
                RData::AAAA(aaaa) => IpAddr::V6(aaaa.0),
                _ => continue,
            };
            let owner = record.name.to_utf8();
            for name in [qname.as_deref(), Some(owner.as_str())]
                .into_iter()
                .flatten()
            {
                let host = name.trim_end_matches('.').to_string();
                if !host.is_empty() {
                    self.record(ip, host);
                }
            }
        }
    }
}

/// Run the policy-enforcing forwarding DNS server until cancelled. Binds UDP on
/// `bind` and forwards approved queries to `upstream`.
pub async fn serve(bind: SocketAddr, upstream: SocketAddr, state: Arc<GatewayState>) -> Result<()> {
    let sock = Arc::new(
        UdpSocket::bind(bind)
            .await
            .with_context(|| format!("binding DNS listener on {bind}"))?,
    );
    tracing::info!(%bind, %upstream, "dns resolver listening (policy-enforced)");

    let mut buf = vec![0u8; 4096];
    loop {
        let (len, client) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "dns recv error");
                continue;
            }
        };
        let query = buf[..len].to_vec();
        let sock = sock.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_query(&sock, client, &query, upstream, &state).await {
                tracing::debug!(error = %e, "dns handling failed");
            }
        });
    }
}

async fn handle_query(
    sock: &UdpSocket,
    client: SocketAddr,
    query: &[u8],
    upstream: SocketAddr,
    state: &GatewayState,
) -> Result<()> {
    match classify_query(query) {
        // Unparseable → drop (fail-closed; never forward).
        QueryGate::Drop => {
            tracing::debug!("dropping unparseable DNS query");
            Ok(())
        }
        // 0 or many questions → refuse locally.
        QueryGate::Refuse => {
            sock.send_to(&refused(query), client).await?;
            Ok(())
        }
        QueryGate::Resolve { qname, qtype } => match state.decide_dns(&qname, &qtype).await {
            Verdict::Allow => forward(sock, client, query, upstream, state).await,
            // Deny (or an unresolved ask) → refuse locally; never touch upstream.
            _ => {
                sock.send_to(&refused(query), client).await?;
                Ok(())
            }
        },
    }
}

/// What to do with a raw query before policy — the pure, testable pre-resolution
/// gate. Fail-closed: anything we can't cleanly parse as a single-question query
/// is dropped or refused, never forwarded.
#[derive(Debug, PartialEq, Eq)]
enum QueryGate {
    /// Unparseable bytes — drop silently.
    Drop,
    /// Parseable but not a single-question query (0 or many) — REFUSE.
    Refuse,
    /// A single question to gate by policy.
    Resolve { qname: String, qtype: String },
}

fn classify_query(query: &[u8]) -> QueryGate {
    match Message::from_vec(query) {
        Err(_) => QueryGate::Drop,
        Ok(msg) if msg.queries.len() != 1 => QueryGate::Refuse,
        Ok(msg) => {
            let q = &msg.queries[0];
            QueryGate::Resolve {
                qname: q.name().to_utf8(),
                qtype: q.query_type().to_string(),
            }
        }
    }
}

/// Forward an approved query upstream, record answers, and relay the response.
async fn forward(
    sock: &UdpSocket,
    client: SocketAddr,
    query: &[u8],
    upstream: SocketAddr,
    state: &GatewayState,
) -> Result<()> {
    let bind: SocketAddr = if upstream.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let up = UdpSocket::bind(bind).await?;
    up.send_to(query, upstream).await?;

    let mut resp = vec![0u8; 4096];
    let len = tokio::time::timeout(std::time::Duration::from_secs(5), up.recv(&mut resp)).await??;
    let resp = &resp[..len];

    if let Ok(msg) = Message::from_vec(resp) {
        state.dns().record_answers(&msg);
    }
    sock.send_to(resp, client).await?;
    Ok(())
}

/// Build a REFUSED response that echoes the query's id, op_code, and question(s).
/// Best-effort: an unparseable query yields no response (caller drops instead).
fn refused(query: &[u8]) -> Vec<u8> {
    let Ok(msg) = Message::from_vec(query) else {
        return Vec::new();
    };
    let mut resp = Message::error_msg(msg.id, msg.op_code, ResponseCode::Refused);
    for q in &msg.queries {
        resp.add_query(q.clone());
    }
    resp.to_vec().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_records_and_looks_up() {
        let map = DnsMap::new();
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        map.record(ip, "example.com".into());
        assert_eq!(map.lookup_all(ip), vec!["example.com".to_string()]);
        assert!(map.lookup_all("1.1.1.1".parse().unwrap()).is_empty());
    }

    use hickory_proto::op::{MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    fn query_bytes(questions: &[(&str, RecordType)]) -> Vec<u8> {
        let mut m = Message::new(0x1234, MessageType::Query, OpCode::Query);
        for (name, rt) in questions {
            m.add_query(Query::query(Name::from_ascii(name).unwrap(), *rt));
        }
        m.to_vec().unwrap()
    }

    #[test]
    fn classify_drops_unparseable() {
        assert_eq!(classify_query(b"not a dns message"), QueryGate::Drop);
        assert_eq!(classify_query(&[]), QueryGate::Drop);
    }

    #[test]
    fn classify_refuses_zero_or_many_questions() {
        // A DNS-tunnel/amplification trick is to pack multiple questions; gate them.
        assert_eq!(classify_query(&query_bytes(&[])), QueryGate::Refuse);
        assert_eq!(
            classify_query(&query_bytes(&[
                ("a.example.", RecordType::A),
                ("b.example.", RecordType::A),
            ])),
            QueryGate::Refuse
        );
    }

    #[test]
    fn classify_resolves_single_question() {
        match classify_query(&query_bytes(&[("api.github.com.", RecordType::A)])) {
            QueryGate::Resolve { qname, qtype } => {
                assert_eq!(qname, "api.github.com.");
                assert_eq!(qtype, "A");
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn record_answers_maps_ip_to_the_queried_name_through_cname() {
        use hickory_proto::rr::rdata::{A, CNAME};
        use hickory_proto::rr::Record;
        use std::net::Ipv4Addr;

        // files.pythonhosted.org CNAME …fastly.net A 1.2.3.4 — an allow-list names
        // the alias, never the canonical CDN owner, so the IP must be attributed to
        // the queried name (the regression: it was attributed only to the owner).
        let map = DnsMap::new();
        let mut m = Message::new(1, MessageType::Response, OpCode::Query);
        m.add_query(Query::query(
            Name::from_ascii("files.pythonhosted.org.").unwrap(),
            RecordType::A,
        ));
        m.add_answer(Record::from_rdata(
            Name::from_ascii("files.pythonhosted.org.").unwrap(),
            300,
            RData::CNAME(CNAME(Name::from_ascii("dukxyz.fastly.net.").unwrap())),
        ));
        m.add_answer(Record::from_rdata(
            Name::from_ascii("dukxyz.fastly.net.").unwrap(),
            300,
            RData::A(A(Ipv4Addr::new(1, 2, 3, 4))),
        ));
        map.record_answers(&m);

        let names = map.lookup_all("1.2.3.4".parse().unwrap());
        assert!(
            names.contains(&"files.pythonhosted.org".to_string()),
            "IP must be attributed to the queried (allow-listed) name; got {names:?}"
        );
    }

    #[test]
    fn map_keeps_multiple_names_per_ip() {
        // A shared CDN IP fronts several hosts; all recently-resolved names are kept
        // so a connection can be authorized if any of them is allowed.
        let map = DnsMap::new();
        let ip: IpAddr = "104.16.0.1".parse().unwrap();
        map.record(ip, "allowed.example".into());
        map.record(ip, "other.example".into());
        map.record(ip, "allowed.example".into()); // dedup, refresh
        let names = map.lookup_all(ip);
        assert!(names.contains(&"allowed.example".to_string()));
        assert!(names.contains(&"other.example".to_string()));
        assert_eq!(names.len(), 2, "duplicate name is deduped");
    }
}
