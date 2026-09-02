//! Wire types shared between the host `cowboy` process and the `cowboy-gateway`
//! binary over the unix control socket.
//!
//! Framing: newline-delimited JSON. Each line is one [`ControlMessage`]. The
//! gateway is the client (connects to the host-owned socket); the host is the
//! server that renders "ask" prompts and returns verdicts.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// How long (seconds) either end waits for a network-approval verdict before
/// failing closed. Shared so the gateway (waiting on the host control socket)
/// and the host worker (waiting on the user) use the *same* budget — if they
/// disagreed, one could give up while the other still waited (leaving a stale
/// modal while the agent moves on).
///
/// This is a *human* gate, so the window is generous: with a client attached the
/// agent's command simply blocks on the prompt until you decide (or you interrupt
/// the turn). It is NOT a liveness timeout — a headless run with no client
/// attached is denied immediately by the host (it never waits), so this long
/// budget only ever applies when someone is actually there to answer.
pub const APPROVAL_TIMEOUT_SECS: u64 = 1800;

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
    /// A DNS query (resolution gated at the gateway's resolver, port 53). The
    /// `host` is the queried name.
    Dns,
}

/// A single outbound connection attempt observed by the policy engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAttempt {
    pub protocol: Protocol,
    /// Hostname recovered from SNI, Host header, CONNECT target, or DNS map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpAddr>,
    pub port: u16,
    /// PID of the sandboxed command that opened the connection, as seen in the
    /// sandbox's own PID namespace.
    ///
    /// Attribution, not identity: it lets a prompt say *which* concurrent command
    /// wants a destination, which matters when several subagents run at once. This is
    /// a new capability rather than a restored one — under Docker every command
    /// shared one uid, so nothing distinguished them either.
    ///
    /// Never used for an authorization decision. It is reported by the relay, which
    /// is inside the boundary, so it is only ever as trustworthy as a label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_pid: Option<u32>,
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

/// Serialize a message as a single newline-terminated JSON line.
pub fn encode_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("control message serializes");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `command_pid` crosses the relay boundary as JSON, so its shape is a
    /// contract — and it must stay optional so an attempt without attribution
    /// still parses.
    #[test]
    fn attempt_round_trips_with_and_without_a_command_pid() {
        for pid in [None, Some(4242u32)] {
            let a = NetworkAttempt {
                protocol: Protocol::Tls,
                host: Some("github.com".into()),
                ip: None,
                port: 443,
                command_pid: pid,
            };
            let line = encode_line(&a);
            assert!(line.ends_with('\n'));
            let back: NetworkAttempt = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(a, back);
        }
        // Absent in the JSON entirely -> None, not an error.
        let a: NetworkAttempt = serde_json::from_str(r#"{"protocol":"tls","port":443}"#).unwrap();
        assert_eq!(a.command_pid, None);
    }

    #[test]
    fn attempt_label() {
        let a = NetworkAttempt {
            protocol: Protocol::Tcp,
            host: None,
            ip: Some("1.2.3.4".parse().unwrap()),
            port: 22,
            command_pid: None,
        };
        assert_eq!(a.label(), "1.2.3.4:22");
    }
}
