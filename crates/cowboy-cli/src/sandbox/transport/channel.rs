//! The relay ↔ policy-engine channel: **the enforcement boundary**.
//!
//! Everything else in the sandbox is contained by the kernel — namespaces,
//! Landlock, seccomp, an empty capability bounding set. This channel is different:
//! the relay *reports* a connection's original destination, and the engine trusts
//! that report when it decides. Forge the report and every domain rule is defeated
//! with all the kernel controls perfectly intact.
//!
//! So the channel is an **anonymous `socketpair`**, inherited across fork. It has no
//! name in the filesystem and none in any abstract namespace, so it cannot be
//! opened, connected to, or enumerated: reaching it requires already holding the
//! file descriptor. Three independent controls back that up, none of which the agent
//! can influence:
//!
//! - the relay lives in a different PID namespace from every agent command, so no
//!   agent process can even see it, let alone read its `/proc/<pid>/fd`;
//! - agent commands run with an empty capability bounding set;
//! - `ptrace` is refused by the seccomp filter, and yama `ptrace_scope=1` would
//!   restrict it to descendants anyway — and the relay is nobody's descendant.
//!
//! Deliberately **not** dependent on `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`. A named
//! abstract socket would need that scope to be safe, which would make a Landlock ABI
//! feature load-bearing for the one boundary that must never degrade. A nameless
//! socket needs no such rule.
//!
//! What crosses it: on one channel, a request describing one connection and a reply
//! that is either a refusal or an already-connected file descriptor; on the other, a
//! DNS query as opaque bytes and the response bytes to hand back. The descriptor is
//! created by the engine in the **host** network namespace, and a passed descriptor
//! keeps the namespace it was created in — verified, and the reason the relay never
//! needs to create an outbound socket of its own. That in turn is why there is no uid
//! exemption in the nftables ruleset to get wrong.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The file descriptors the relay inherits its channels on.
///
/// Fixed by convention rather than passed in environment variables, because the
/// environment of a process is readable and this is two fewer things to name.
///
/// There are **two** channels, not one multiplexed channel, and the reason is
/// framing. A `SEQPACKET` pair carries no request ids, so a reply belongs to
/// whichever exchange holds the socket — which means each channel must be used one
/// exchange at a time. Sharing a single channel would therefore put every connection
/// decision behind whatever DNS query happened to be waiting on an upstream resolver
/// (seconds, in the bad case). The alternative, adding request ids and a
/// demultiplexer, puts framing logic on the boundary that decides egress; a second
/// nameless socket costs a descriptor and no logic at all.
pub const CONNECT_FD: RawFd = 3;
pub const RESOLVE_FD: RawFd = 4;

/// Largest datagram we will read. Requests carry either a small peek of the client's
/// first bytes or a DNS message, both bounded well below this.
///
/// Sized so a maximum DNS datagram survives hex encoding (which doubles it) with room
/// for the JSON around it. A `SEQPACKET` read into a buffer smaller than the datagram
/// would *truncate* it, which on this boundary would be silent corruption.
pub const MAX_MESSAGE: usize = 16384;

/// How many bytes of the client's first write the relay peeks and forwards.
///
/// Enough for a TLS ClientHello's SNI or an HTTP request line plus Host header.
/// Peeked with `MSG_PEEK`, so the bytes stay in the socket for the splice — the
/// destination must receive the client's stream byte-for-byte.
pub const PEEK_BYTES: usize = 2048;

/// One connection the relay wants a decision about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectRequest {
    /// The destination the client actually dialled, recovered with
    /// `SO_ORIGINAL_DST` before the nat rewrite is visible to the application.
    pub dst_ip: String,
    pub dst_port: u16,
    /// The client's first bytes, for protocol classification (TLS SNI, HTTP Host).
    ///
    /// Used to *classify*, never to authorize: the name a client presents is chosen
    /// by the agent. Authorization uses the name the resolver itself recorded for
    /// the destination IP.
    #[serde(with = "hex_bytes")]
    pub peek: Vec<u8>,
    /// Which sandboxed command opened the connection, if it could be determined.
    ///
    /// A label for the prompt, so a user facing several concurrent subagents knows
    /// which one is asking. Best-effort and never authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_pid: Option<u32>,
}

/// The engine's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectReply {
    /// True when a connected descriptor accompanies this message.
    pub allowed: bool,
    /// Why, for the relay's log and for the error the client ultimately sees.
    pub reason: String,
}

/// One DNS query the sandbox wants answered.
///
/// The query travels as **opaque bytes**. Nothing inside the sandbox parses it, gates
/// it, or rewrites it: the relay reads a datagram off its loopback socket and forwards
/// it, so there is no DNS policy inside the boundary to bypass. Every decision —
/// record type, name, tunnel shape, and which upstream to ask — is made on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveRequest {
    #[serde(with = "hex_bytes")]
    pub query: Vec<u8>,
    /// Which sandboxed command asked, if it could be determined. A label, never
    /// authoritative — same as [`ConnectRequest::command_pid`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_pid: Option<u32>,
}

/// The engine's answer to a query.
///
/// An empty response means "send nothing back": the query was unparseable or the
/// upstream failed, and the client's own resolver should retry rather than cache a
/// synthesized refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveReply {
    #[serde(with = "hex_bytes")]
    pub response: Vec<u8>,
}

/// Hex rather than base64 to avoid a dependency for a field this small.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("odd-length hex"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// Create the channel: `(engine_end, relay_end)`.
///
/// `SOCK_SEQPACKET` so each message is framed by the kernel. With a stream socket we
/// would have to length-prefix and re-frame, and a framing bug on the boundary that
/// decides egress policy is not a bug worth risking.
pub fn pair() -> Result<(OwnedFd, OwnedFd)> {
    use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
    let (a, b) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .context("creating the relay channel socketpair")?;
    Ok((a, b))
}

/// The policy engine's ends of the relay's two channels.
///
/// Held by the worker. Until both are served, anything the sandbox tries blocks
/// waiting for a verdict — the right failure direction.
pub struct EngineChannels {
    /// Connection decisions, answered with a connected descriptor.
    pub connect: OwnedFd,
    /// DNS queries, answered with response bytes.
    pub resolve: OwnedFd,
}

/// Send `msg`, optionally passing `fd` alongside it.
pub fn send(sock: BorrowedFd<'_>, msg: &[u8], fd: Option<BorrowedFd<'_>>) -> Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use std::io::IoSlice;

    let iov = [IoSlice::new(msg)];
    let fds = fd.map(|f| [f.as_raw_fd()]);
    let cmsgs: Vec<ControlMessage> = match &fds {
        Some(f) => vec![ControlMessage::ScmRights(f)],
        None => Vec::new(),
    };
    sendmsg::<()>(sock.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .context("sending on the relay channel")?;
    Ok(())
}

/// Receive one message, and a descriptor if one was passed.
///
/// Returns `Ok(None)` at end of stream, so the caller can distinguish "the peer went
/// away" from an error and shut down cleanly.
pub fn recv(sock: BorrowedFd<'_>) -> Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    use std::io::IoSliceMut;

    let mut buf = vec![0u8; MAX_MESSAGE];
    let mut cmsg_space = nix::cmsg_space!([RawFd; 1]);
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_space),
        MsgFlags::empty(),
    )
    .context("receiving on the relay channel")?;

    if msg.bytes == 0 {
        return Ok(None); // peer closed
    }

    let mut received = None;
    for c in msg.cmsgs().context("reading control messages")? {
        if let ControlMessageOwned::ScmRights(fds) = c {
            for fd in fds {
                if received.is_none() {
                    // SAFETY: the kernel just installed this descriptor for us and
                    // no one else holds it.
                    received = Some(unsafe { OwnedFd::from_raw_fd(fd) });
                } else {
                    // More than one descriptor is a protocol violation; close the
                    // extras rather than leaking them.
                    // SAFETY: same, and we immediately drop it.
                    drop(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
    }
    let n = msg.bytes;
    buf.truncate(n);
    Ok(Some((buf, received)))
}

/// Encode a value as one datagram.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let v = serde_json::to_vec(value).context("encoding a relay channel message")?;
    if v.len() > MAX_MESSAGE {
        bail!("relay channel message too large ({} bytes)", v.len());
    }
    Ok(v)
}

/// Decode one datagram.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).context("decoding a relay channel message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn request() -> ConnectRequest {
        ConnectRequest {
            dst_ip: "93.184.216.34".into(),
            dst_port: 443,
            peek: vec![0x16, 0x03, 0x01, 0x00, 0xff],
            command_pid: Some(4242),
        }
    }

    #[test]
    fn a_request_round_trips() {
        let r = request();
        let bytes = encode(&r).unwrap();
        assert_eq!(decode::<ConnectRequest>(&bytes).unwrap(), r);
    }

    /// The peek is arbitrary binary (a TLS ClientHello), so it must survive
    /// encoding exactly — a lossy round trip would corrupt SNI classification.
    #[test]
    fn arbitrary_binary_peek_survives_encoding() {
        let mut r = request();
        r.peek = (0u16..=255).map(|b| b as u8).collect();
        let bytes = encode(&r).unwrap();
        assert_eq!(decode::<ConnectRequest>(&bytes).unwrap().peek, r.peek);
    }

    #[test]
    fn an_absent_command_pid_still_decodes() {
        let r: ConnectRequest = decode(br#"{"dst_ip":"1.2.3.4","dst_port":80,"peek":""}"#).unwrap();
        assert_eq!(r.command_pid, None);
        assert!(r.peek.is_empty());
    }

    #[test]
    fn an_oversized_message_is_refused_rather_than_truncated() {
        let mut r = request();
        r.peek = vec![0u8; MAX_MESSAGE];
        assert!(
            encode(&r).is_err(),
            "silently truncating a boundary message would be worse than failing"
        );
    }

    /// A `SEQPACKET` read into too small a buffer truncates the datagram silently, so
    /// the largest DNS message the resolver will handle must fit *after* hex encoding
    /// doubles it. This is the check that keeps [`MAX_MESSAGE`] honest.
    #[test]
    fn a_maximum_size_dns_message_fits_a_channel_datagram() {
        let r = ResolveRequest {
            query: vec![0xab; cowboy_gateway::dns::MAX_DATAGRAM],
            command_pid: Some(1),
        };
        let bytes = encode(&r).expect("a maximum-size DNS datagram must fit");
        assert!(bytes.len() <= MAX_MESSAGE);
        assert_eq!(decode::<ResolveRequest>(&bytes).unwrap(), r);

        let reply = ResolveReply {
            response: vec![0xcd; cowboy_gateway::dns::MAX_DATAGRAM],
        };
        let bytes = encode(&reply).expect("a maximum-size DNS response must fit");
        assert_eq!(decode::<ResolveReply>(&bytes).unwrap(), reply);
    }

    /// An empty response is the "drop it" signal, so it must survive the round trip
    /// as *empty* rather than becoming an error or a stray byte.
    #[test]
    fn an_empty_dns_response_round_trips_as_a_drop() {
        let reply = ResolveReply {
            response: Vec::new(),
        };
        let bytes = encode(&reply).unwrap();
        assert!(decode::<ResolveReply>(&bytes).unwrap().response.is_empty());
    }

    #[test]
    fn messages_cross_the_channel() {
        let (engine, relay) = pair().unwrap();
        let req = encode(&request()).unwrap();
        send(relay.as_fd(), &req, None).unwrap();
        let (got, fd) = recv(engine.as_fd()).unwrap().expect("a message");
        assert_eq!(decode::<ConnectRequest>(&got).unwrap(), request());
        assert!(fd.is_none());
    }

    /// The mechanism the whole design rests on: a descriptor created on one side is
    /// usable on the other.
    #[test]
    fn a_descriptor_crosses_the_channel_and_still_works() {
        use std::io::{Read, Write};
        let (engine, relay) = pair().unwrap();

        // Stand in for "a connection the engine dialled": a socketpair whose far
        // end we can write to.
        let (near, far) = pair().unwrap();
        let reply = encode(&ConnectReply {
            allowed: true,
            reason: "test".into(),
        })
        .unwrap();
        send(engine.as_fd(), &reply, Some(near.as_fd())).unwrap();

        let (got, passed) = recv(relay.as_fd()).unwrap().expect("a message");
        assert!(decode::<ConnectReply>(&got).unwrap().allowed);
        let passed = passed.expect("a descriptor should have been passed");

        // Write on the original far end; read through the descriptor that crossed.
        let mut far_file = std::fs::File::from(far);
        far_file.write_all(b"hello through a passed fd").unwrap();
        let mut near_file = std::fs::File::from(passed);
        let mut buf = [0u8; 64];
        let n = near_file.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello through a passed fd");
    }

    #[test]
    fn a_closed_peer_reads_as_end_of_stream_not_an_error() {
        let (engine, relay) = pair().unwrap();
        drop(relay);
        assert!(
            recv(engine.as_fd()).unwrap().is_none(),
            "a departed peer must be distinguishable from a failure"
        );
    }

    /// The channel must have no name anywhere: that is what makes it unreachable
    /// rather than merely protected.
    #[test]
    fn the_channel_is_anonymous() {
        use nix::sys::socket::{getsockname, UnixAddr};
        let (engine, _relay) = pair().unwrap();
        let addr: UnixAddr = getsockname(engine.as_raw_fd()).unwrap();
        assert!(
            addr.path().is_none(),
            "a filesystem path would make the boundary openable: {addr:?}"
        );
        assert!(
            addr.as_abstract().is_none(),
            "an abstract name would make the boundary connectable, and would make \
             Landlock ABI 6 scoping load-bearing for it: {addr:?}"
        );
    }
}
