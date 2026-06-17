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
use std::time::Instant;

use anyhow::{Context, Result};
use hickory_proto::op::{Message, ResponseCode};
use hickory_proto::rr::RData;
use tokio::net::UdpSocket;

use crate::state::GatewayState;
use cowboy_core::netproto::Verdict;

/// Shared, thread-safe map of resolved IP -> hostname.
#[derive(Debug, Clone, Default)]
pub struct DnsMap {
    inner: Arc<Mutex<HashMap<IpAddr, (String, Instant)>>>,
}

impl DnsMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, ip: IpAddr, host: String) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ip, (host, Instant::now()));
    }

    /// Look up the hostname most recently resolved to `ip`.
    pub fn lookup(&self, ip: IpAddr) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&ip)
            .map(|(h, _)| h.clone())
    }

    /// Record every A/AAAA answer in a DNS response message.
    pub fn record_answers(&self, msg: &Message) {
        for record in &msg.answers {
            let name = record.name.to_utf8();
            let host = name.trim_end_matches('.').to_string();
            if host.is_empty() {
                continue;
            }
            match &record.data {
                RData::A(a) => self.record(IpAddr::V4(a.0), host),
                RData::AAAA(aaaa) => self.record(IpAddr::V6(aaaa.0), host),
                _ => {}
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
    // Parse the query. Unparseable → drop (fail-closed; never forward).
    let Ok(msg) = Message::from_vec(query) else {
        tracing::debug!("dropping unparseable DNS query");
        return Ok(());
    };
    // Exactly one question is the norm; 0 or many → refuse.
    let Some(q) = (msg.queries.len() == 1).then(|| &msg.queries[0]) else {
        sock.send_to(&refused(&msg), client).await?;
        return Ok(());
    };
    let qname = q.name().to_utf8();
    let qtype = q.query_type().to_string();

    match state.decide_dns(&qname, &qtype).await {
        Verdict::Allow => forward(sock, client, query, upstream, state).await,
        // Deny (or an unresolved ask) → refuse locally; never touch upstream.
        _ => {
            sock.send_to(&refused(&msg), client).await?;
            Ok(())
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

/// Build a REFUSED response that echoes the query's id, op_code, and question.
fn refused(query: &Message) -> Vec<u8> {
    let mut resp = Message::error_msg(query.id, query.op_code, ResponseCode::Refused);
    for q in &query.queries {
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
        assert_eq!(map.lookup(ip), Some("example.com".to_string()));
        assert_eq!(map.lookup("1.1.1.1".parse().unwrap()), None);
    }
}
