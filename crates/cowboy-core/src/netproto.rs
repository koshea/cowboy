//! Wire types shared between the host `cowboy` process and the `cowboy-gateway`
//! binary over the unix control socket.
//!
//! Framing: newline-delimited JSON. Each line is one [`ControlMessage`]. The
//! gateway is the client (connects to the host-owned socket); the host is the
//! server that renders "ask" prompts and returns verdicts.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Transport-layer protocol of an outbound attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// TLS over TCP (port 443 etc.) — destination known via SNI.
    Tls,
    /// Plaintext HTTP — destination known via Host header / CONNECT.
    Http,
    /// Raw TCP with no recovered hostname.
    Tcp,
}

/// A single outbound connection attempt observed by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAttempt {
    pub protocol: Protocol,
    /// Hostname recovered from SNI, Host header, CONNECT target, or DNS map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpAddr>,
    pub port: u16,
}

impl NetworkAttempt {
    /// A human-readable destination label for prompts and logs.
    pub fn label(&self) -> String {
        match (&self.host, self.ip) {
            (Some(h), _) => format!("{h}:{}", self.port),
            (None, Some(ip)) => format!("{ip}:{}", self.port),
            (None, None) => format!("?:{}", self.port),
        }
    }
}

/// The verdict for an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    Deny,
    Ask,
}

/// How long an approval persists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalScope {
    Once,
    Session,
    Project,
    Global,
}

/// Messages sent from the gateway to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GatewayMessage {
    /// Request a decision for an attempt the policy classified as `ask`.
    Ask { id: u64, attempt: NetworkAttempt },
    /// Informational: a decision the gateway already made (for the activity log).
    Event {
        attempt: NetworkAttempt,
        verdict: Verdict,
        reason: String,
    },
}

/// Messages sent from the host back to the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostMessage {
    /// Verdict for a prior [`GatewayMessage::Ask`].
    Decision {
        id: u64,
        verdict: Verdict,
        scope: ApprovalScope,
    },
}

/// Either direction, for generic framing helpers/tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlMessage {
    FromGateway(GatewayMessage),
    FromHost(HostMessage),
}

/// Serialize a message as a single newline-terminated JSON line.
pub fn encode_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("control message serializes");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_roundtrips() {
        let msg = GatewayMessage::Ask {
            id: 7,
            attempt: NetworkAttempt {
                protocol: Protocol::Tls,
                host: Some("github.com".into()),
                ip: None,
                port: 443,
            },
        };
        let line = encode_line(&msg);
        assert!(line.ends_with('\n'));
        let back: GatewayMessage = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn decision_roundtrips() {
        let msg = HostMessage::Decision {
            id: 7,
            verdict: Verdict::Allow,
            scope: ApprovalScope::Session,
        };
        let back: HostMessage = serde_json::from_str(encode_line(&msg).trim()).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn attempt_label() {
        let a = NetworkAttempt {
            protocol: Protocol::Tcp,
            host: None,
            ip: Some("1.2.3.4".parse().unwrap()),
            port: 22,
        };
        assert_eq!(a.label(), "1.2.3.4:22");
    }
}
