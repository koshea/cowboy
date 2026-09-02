//! Policy-enforcing forwarding DNS resolver.
//!
//! The sandbox can only resolve names through here: its network namespace holds no
//! route to any resolver, and every packet to port 53 is redirected to the relay,
//! which forwards the query bytes to this code on the host. Every query is gated by
//! the policy *before* it leaves: only names the policy Allows or the user approves
//! are forwarded upstream; denied names, disallowed record types, and suspected
//! tunnels are answered REFUSED without a byte going out. This closes DNS as an
//! exfiltration channel.
//!
//! It is also what makes domain rules work at all. Answers are recorded as
//! `ip -> name`, and the relay admits a connection because the resolver mapped that
//! IP to an allowed name — so `allow: github.com` is enforced here, at resolution,
//! not by trusting anything the agent later says about where it is connecting.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::RData;
use tokio::net::UdpSocket;

use crate::state::GatewayState;
use cowboy_core::netproto::Verdict;

/// Largest DNS datagram we send or accept. Matches the usual EDNS0 ceiling with
/// room to spare; a larger response arrives truncated and the client retries.
pub const MAX_DATAGRAM: usize = 4096;

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

/// Decide and, if allowed, resolve one query — the whole policy-enforcing resolver
/// as a single host-side function.
///
/// Returns the response datagram to hand back to the client, or `None` when the
/// query must simply be dropped. Deliberately takes bytes and returns bytes rather
/// than owning a socket: the query arrives from the sandbox over the relay channel,
/// while `upstream` must be dialled in the **host** network namespace, so the two
/// ends live in different namespaces and no single socket could serve both.
///
/// Every decision in here is host-side and therefore outside the agent's reach. The
/// sandbox end of this path is a dumb pipe: it forwards bytes and returns bytes, and
/// has no way to resolve a name this function refuses.
pub async fn resolve(query: &[u8], upstream: SocketAddr, state: &GatewayState) -> Option<Vec<u8>> {
    match classify_query(query) {
        // Unparseable → drop (fail-closed; never forward).
        QueryGate::Drop => {
            tracing::debug!("dropping unparseable DNS query");
            None
        }
        // 0 or many questions → refuse locally.
        QueryGate::Refuse => Some(refused(query)),
        QueryGate::Resolve { qname, qtype } => match state.decide_dns(&qname, &qtype).await {
            Verdict::Allow => match forward(query, upstream, state).await {
                Ok(response) => Some(response),
                Err(e) => {
                    // An upstream failure is not a policy decision, so it must not
                    // masquerade as one: dropping lets the client's own resolver
                    // retry, where REFUSED would be cached as an answer.
                    tracing::debug!(error = %e, "dns upstream failed");
                    None
                }
            },
            // Deny (or an unresolved ask) → refuse locally; never touch upstream.
            _ => Some(refused(query)),
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

/// Forward an approved query upstream, record answers, and return the response.
///
/// The response is what authorizes egress: `record_answers` maps the returned IPs
/// to the allow-listed name, and the relay later admits a connection to those IPs
/// *because* of that mapping. So a forged answer would let the agent bind any IP it
/// likes to an allowed name, and this is the code that must make that impossible.
///
/// The sandbox cannot reach this socket at all — it is created here, in the host
/// network namespace, and the agent's namespace has no route to it. What remains is
/// off-path spoofing by anything between here and the resolver, and the fact that
/// the agent *authored* the query, so it knows the transaction id and question it
/// would need to forge. Two defences, both required:
///   1. `connect()` the upstream socket, so the kernel drops datagrams from any
///      source other than the resolver;
///   2. re-read until a reply actually matches the query we sent (id + question), so
///      a datagram that races in from the resolver's own address cannot carry an
///      answer to a different question than the one policy approved.
///
/// Only a reply that passed both is recorded, and only its A/AAAA records.
async fn forward(query: &[u8], upstream: SocketAddr, state: &GatewayState) -> Result<Vec<u8>> {
    const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let bind: SocketAddr = if upstream.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let up = UdpSocket::bind(bind).await?;
    // Connected UDP: the kernel delivers only datagrams whose source is `upstream`.
    up.connect(upstream).await?;
    up.send(query).await?;

    let sent = Message::from_vec(query).context("forwarding an unparseable query")?;
    let deadline = tokio::time::Instant::now() + UPSTREAM_TIMEOUT;
    loop {
        let mut resp = vec![0u8; MAX_DATAGRAM];
        let len = tokio::time::timeout_at(deadline, up.recv(&mut resp)).await??;
        resp.truncate(len);

        let Ok(msg) = Message::from_vec(&resp) else {
            continue; // garbage from the resolver: keep waiting for a real reply
        };
        if !reply_matches(&sent, &msg) {
            tracing::debug!("discarding a DNS reply that does not match the query");
            continue;
        }
        state.dns().record_answers(&msg);
        return Ok(resp);
    }
}

/// Whether `reply` actually answers `sent`: same transaction id, and the same
/// single question (name + type + class). Guards the IP→name map against a reply
/// that answers a *different* question than the one policy approved.
fn reply_matches(sent: &Message, reply: &Message) -> bool {
    if sent.metadata.id != reply.metadata.id || reply.metadata.message_type != MessageType::Response
    {
        return false;
    }
    match (sent.queries.as_slice(), reply.queries.as_slice()) {
        ([q], [r]) => {
            q.name() == r.name()
                && q.query_type() == r.query_type()
                && q.query_class() == r.query_class()
        }
        _ => false,
    }
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

/// The resolver this machine uses, read from `/etc/resolv.conf`.
///
/// Deliberately no fallback to a public resolver. Silently sending a user's lookups
/// to a third party because their config could not be read would be a surprising
/// default, and DNS failing is already the safe direction: names stop resolving, so
/// nothing new becomes reachable.
///
/// A loopback stub (`127.0.0.53`, systemd-resolved) is fine — this is dialled from
/// the host network namespace, where that stub is exactly the right answer.
pub fn host_resolver() -> Option<SocketAddr> {
    let text = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    upstream_from_resolv_conf(&text)
}

/// Parse the first usable `nameserver` line. Split out so it is testable without
/// depending on the machine's configuration.
pub fn upstream_from_resolv_conf(text: &str) -> Option<SocketAddr> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        // Strip any scope suffix (`fe80::1%eth0`), which is not part of the address.
        let addr = parts.next()?.split('%').next().unwrap_or_default();
        if let Ok(ip) = addr.parse::<IpAddr>() {
            return Some(SocketAddr::new(ip, 53));
        }
    }
    None
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

    /// Build a response for `query`, optionally lying about the id or the
    /// question, carrying one A record for `ip`.
    fn reply_for(query: &[u8], id: Option<u16>, qname: Option<&str>, ip: &str) -> Message {
        use hickory_proto::rr::rdata::A;
        use hickory_proto::rr::Record;
        let sent = Message::from_vec(query).unwrap();
        let q = &sent.queries[0];
        let name = match qname {
            Some(n) => Name::from_ascii(n).unwrap(),
            None => q.name().clone(),
        };
        let mut m = Message::new(
            id.unwrap_or(sent.metadata.id),
            MessageType::Response,
            OpCode::Query,
        );
        m.add_query(Query::query(name.clone(), q.query_type()));
        m.add_answer(Record::from_rdata(
            name,
            60,
            RData::A(A(ip.parse().unwrap())),
        ));
        m
    }

    /// The agent authors the query, so it knows the transaction id — the reply
    /// check must therefore bind an answer to the *exact* question that policy
    /// approved, or a forged/mismatched answer could map an attacker IP onto an
    /// allow-listed name and buy the agent egress to it.
    #[test]
    fn reply_matches_only_the_exact_query_it_answers() {
        let q = query_bytes(&[("allowed.example.", RecordType::A)]);
        let sent = Message::from_vec(&q).unwrap();

        let good = reply_for(&q, None, None, "93.184.216.34");
        assert!(reply_matches(&sent, &good), "the resolver's real answer");

        // Wrong transaction id.
        let wrong_id = reply_for(&q, Some(0x9999), None, "6.6.6.6");
        assert!(!reply_matches(&sent, &wrong_id));

        // Right id, but answering a DIFFERENT name than the one we gated.
        let wrong_name = reply_for(&q, None, Some("evil.example."), "6.6.6.6");
        assert!(!reply_matches(&sent, &wrong_name));

        // A query (not a response) echoed back.
        let mut not_a_response = good.clone();
        not_a_response.metadata.message_type = MessageType::Query;
        assert!(!reply_matches(&sent, &not_a_response));
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

    #[test]
    fn the_first_nameserver_is_taken_from_resolv_conf() {
        let text = "# a comment\nsearch example.com\nnameserver 127.0.0.53\nnameserver 1.1.1.1\n";
        assert_eq!(
            upstream_from_resolv_conf(text),
            Some("127.0.0.53:53".parse().unwrap()),
            "a loopback stub is correct here: this is dialled from the host namespace"
        );
    }

    #[test]
    fn resolv_conf_oddities_do_not_yield_a_wrong_resolver() {
        // No nameserver at all → None, and the caller must refuse rather than
        // silently substitute a public resolver.
        assert_eq!(upstream_from_resolv_conf("options edns0\n"), None);
        assert_eq!(upstream_from_resolv_conf(""), None);
        // A commented-out nameserver is not a nameserver.
        assert_eq!(upstream_from_resolv_conf("#nameserver 1.1.1.1\n"), None);
        // A scope suffix is not part of the address.
        assert_eq!(
            upstream_from_resolv_conf("nameserver fe80::1%eth0\n"),
            Some("[fe80::1]:53".parse().unwrap())
        );
        // Garbage is skipped in favour of the next usable line.
        assert_eq!(
            upstream_from_resolv_conf("nameserver not-an-ip\nnameserver 9.9.9.9\n"),
            Some("9.9.9.9:53".parse().unwrap())
        );
    }

    /// A stand-in upstream resolver: answers every query with one A record for
    /// `answer`, echoing the id and question so `reply_matches` accepts it.
    async fn fake_upstream(answer: &'static str) -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            while let Ok((len, from)) = sock.recv_from(&mut buf).await {
                let reply = reply_for(&buf[..len], None, None, answer);
                let _ = sock.send_to(&reply.to_vec().unwrap(), from).await;
            }
        });
        addr
    }

    fn response_code(bytes: &[u8]) -> ResponseCode {
        Message::from_vec(bytes).unwrap().response_code
    }

    fn policy_allowing(domain: &str) -> cowboy_core::config::NetworkPolicy {
        let mut p = cowboy_core::config::NetworkPolicy::default();
        p.allow.domains.push(domain.into());
        p
    }

    fn state(policy: cowboy_core::config::NetworkPolicy) -> Arc<GatewayState> {
        Arc::new(GatewayState::new(
            policy,
            DnsMap::new(),
            Arc::new(crate::DenyAll),
        ))
    }

    /// The payoff of the whole DNS path: resolving an allow-listed name is what
    /// makes a *domain* rule enforceable, because the answer is what authorizes the
    /// connection that follows.
    #[tokio::test]
    async fn an_allowed_name_resolves_and_its_answer_authorizes_the_ip() {
        let upstream = fake_upstream("93.184.216.34").await;
        let state = state(policy_allowing("allowed.example"));

        let query = query_bytes(&[("allowed.example.", RecordType::A)]);
        let response = resolve(&query, upstream, &state)
            .await
            .expect("an allowed name must resolve");
        assert_eq!(response_code(&response), ResponseCode::NoError);

        let names = state.dns().lookup_all("93.184.216.34".parse().unwrap());
        assert!(
            names.contains(&"allowed.example".to_string()),
            "the answer must map the IP to the allowed name, or the connection to it \
             cannot be authorized; got {names:?}"
        );
    }

    /// A denied name is refused *here*, without a byte leaving — otherwise the query
    /// itself is the exfiltration channel, and there is no later connection to gate.
    #[tokio::test]
    async fn a_denied_name_is_refused_without_reaching_upstream() {
        // Deliberately an address with no listener: if the implementation ever
        // forwarded, this would time out rather than answer promptly.
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut policy = policy_allowing("allowed.example");
        policy.deny.domains.push("evil.example".into());
        let state = state(policy);

        let query = query_bytes(&[("secrets.evil.example.", RecordType::A)]);
        let response =
            tokio::time::timeout(Duration::from_millis(500), resolve(&query, dead, &state))
                .await
                .expect("must refuse locally, not wait on upstream")
                .expect("a REFUSED answer, not a drop");
        assert_eq!(response_code(&response), ResponseCode::Refused);
    }

    /// Deliberate and worth pinning, because it reads like a hole and is not one: an
    /// *unknown* name resolves. A resolver that parked a query on a human prompt
    /// would simply time out, and resolving is not egress — the connection to
    /// whatever it resolved to is gated at connect time, where prompting works and
    /// the verdict can be cached per host.
    #[tokio::test]
    async fn an_unknown_name_resolves_because_the_gate_is_at_connect_time() {
        let upstream = fake_upstream("93.184.216.34").await;
        let state = state(policy_allowing("allowed.example"));

        let query = query_bytes(&[("unknown.example.", RecordType::A)]);
        let response = resolve(&query, upstream, &state)
            .await
            .expect("an unknown name resolves");
        assert_eq!(response_code(&response), ResponseCode::NoError);
    }

    /// TXT is the classic tunnel carrier and is refused even for a name the policy
    /// otherwise allows.
    #[tokio::test]
    async fn a_tunnel_prone_record_type_is_refused_even_for_an_allowed_name() {
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let state = state(policy_allowing("allowed.example"));

        let query = query_bytes(&[("allowed.example.", RecordType::TXT)]);
        let response =
            tokio::time::timeout(Duration::from_millis(500), resolve(&query, dead, &state))
                .await
                .expect("must refuse locally")
                .expect("a REFUSED answer");
        assert_eq!(response_code(&response), ResponseCode::Refused);
    }

    /// Unparseable input is dropped rather than answered: there is no id or question
    /// to echo, and inventing one would be worse than silence.
    #[tokio::test]
    async fn unparseable_input_is_dropped() {
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let state = state(cowboy_core::config::NetworkPolicy::default());
        assert!(resolve(b"not a dns message", dead, &state).await.is_none());
    }

    /// An upstream that never answers must not turn into a REFUSED response: that
    /// would be cached by the client as a policy answer. Dropping lets it retry.
    #[tokio::test]
    async fn an_upstream_failure_is_dropped_not_reported_as_a_refusal() {
        let state = state(policy_allowing("allowed.example"));
        let query = query_bytes(&[("allowed.example.", RecordType::A)]);

        // A resolver that replies with garbage, so `forward` never gets a match and
        // the read deadline is what ends it.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            while let Ok((_, from)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(b"garbage", from).await;
            }
        });

        tokio::time::pause();
        let task = tokio::spawn(async move { resolve(&query, addr, &state).await });
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(
            task.await.unwrap().is_none(),
            "an upstream failure must be a drop, not a synthesized refusal"
        );
    }
}
