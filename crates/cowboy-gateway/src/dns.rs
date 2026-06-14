//! Forwarding DNS resolver.
//!
//! The agent can only resolve names through the gateway (its `--dns` points
//! here and direct egress to other resolvers is dropped). We forward queries
//! upstream, then inspect the answers to record `ip -> domain` so the
//! transparent TCP path can map a connection's destination IP back to the
//! requested hostname for policy decisions.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use hickory_proto::op::Message;
use hickory_proto::rr::RData;
use tokio::net::UdpSocket;

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
            .unwrap()
            .insert(ip, (host, Instant::now()));
    }

    /// Look up the hostname most recently resolved to `ip`.
    pub fn lookup(&self, ip: IpAddr) -> Option<String> {
        self.inner.lock().unwrap().get(&ip).map(|(h, _)| h.clone())
    }

    /// Record every A/AAAA answer in a DNS response message.
    pub fn record_answers(&self, msg: &Message) {
        for record in msg.answers() {
            let name = record.name().to_utf8();
            let host = name.trim_end_matches('.').to_string();
            if host.is_empty() {
                continue;
            }
            match record.data() {
                RData::A(a) => self.record(IpAddr::V4(a.0), host),
                RData::AAAA(aaaa) => self.record(IpAddr::V6(aaaa.0), host),
                _ => {}
            }
        }
    }
}

/// Run the forwarding DNS server until cancelled. Binds UDP on `bind` and
/// forwards to `upstream`.
pub async fn serve(bind: SocketAddr, upstream: SocketAddr, map: DnsMap) -> Result<()> {
    let sock = Arc::new(
        UdpSocket::bind(bind)
            .await
            .with_context(|| format!("binding DNS listener on {bind}"))?,
    );
    tracing::info!(%bind, %upstream, "dns forwarder listening");

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
        let map = map.clone();
        tokio::spawn(async move {
            if let Err(e) = forward_one(&sock, client, &query, upstream, &map).await {
                tracing::debug!(error = %e, "dns forward failed");
            }
        });
    }
}

async fn forward_one(
    sock: &UdpSocket,
    client: SocketAddr,
    query: &[u8],
    upstream: SocketAddr,
    map: &DnsMap,
) -> Result<()> {
    // Ephemeral upstream socket per query.
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

    // Record mappings (best-effort; relay regardless of parse success).
    if let Ok(msg) = Message::from_vec(resp) {
        map.record_answers(&msg);
    }
    sock.send_to(resp, client).await?;
    Ok(())
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
