//! Minimal HTTP parsing for the gateway: the `CONNECT host:port` request line
//! (explicit-proxy path) and the `Host:` header (transparent plaintext path).
//!
//! Untrusted input — fuzzed/property-tested for panics. We never build a full
//! HTTP stack; we read one line / one header set and then splice bytes.

/// A parsed `CONNECT` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
}

/// Parse a `CONNECT authority HTTP/1.x` request line from the buffer.
///
/// Returns `Ok(None)` if more bytes are needed, `Err(())` if it is not a valid
/// CONNECT request.
#[allow(clippy::result_unit_err)]
pub fn parse_connect(buf: &[u8]) -> Result<Option<ConnectTarget>, ()> {
    // Need a complete request line.
    let Some(line_end) = find_crlf(buf) else {
        // Bound the line length so we don't buffer forever on junk.
        if buf.len() > 8192 {
            return Err(());
        }
        return Ok(None);
    };
    let line = std::str::from_utf8(&buf[..line_end]).map_err(|_| ())?;
    let mut parts = line.split(' ');
    let method = parts.next().ok_or(())?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err(());
    }
    let authority = parts.next().ok_or(())?;
    let version = parts.next().ok_or(())?;
    if !version.starts_with("HTTP/") {
        return Err(());
    }
    let (host, port) = split_authority(authority).ok_or(())?;
    Ok(Some(ConnectTarget {
        host: host.to_string(),
        port,
    }))
}

/// Extract the `Host` header value (host only, no port) from buffered request
/// bytes. Returns `Ok(None)` if headers are not yet complete.
#[allow(clippy::result_unit_err)]
pub fn parse_host_header(buf: &[u8]) -> Result<Option<String>, ()> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf) {
        Ok(httparse::Status::Complete(_)) | Ok(httparse::Status::Partial) => {
            for h in req.headers.iter() {
                if h.name.eq_ignore_ascii_case("host") {
                    let raw = std::str::from_utf8(h.value).map_err(|_| ())?;
                    let host = raw.split(':').next().unwrap_or(raw).trim();
                    if host.is_empty() {
                        return Err(());
                    }
                    return Ok(Some(host.to_string()));
                }
            }
            // Headers parsed (or partial) but no Host yet.
            if buf.len() > 8192 {
                return Err(());
            }
            Ok(None)
        }
        Err(_) => Err(()),
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Split `host:port` (default 443 if no port). Supports bracketed IPv6.
fn split_authority(authority: &str) -> Option<(&str, u16)> {
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        // [v6]:port
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => 443,
        };
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host, port.parse().ok()?)),
        _ => Some((authority, 443)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect() {
        let t = parse_connect(b"CONNECT github.com:443 HTTP/1.1\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(t.host, "github.com");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn connect_defaults_port() {
        let t = parse_connect(b"CONNECT example.com HTTP/1.1\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(t.port, 443);
    }

    #[test]
    fn connect_ipv6() {
        let t = parse_connect(b"CONNECT [2606:4700::1111]:8443 HTTP/1.1\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(t.host, "2606:4700::1111");
        assert_eq!(t.port, 8443);
    }

    #[test]
    fn connect_incomplete() {
        assert_eq!(parse_connect(b"CONNECT github.com:443 HTTP"), Ok(None));
    }

    #[test]
    fn rejects_non_connect() {
        assert_eq!(parse_connect(b"GET / HTTP/1.1\r\n"), Err(()));
    }

    #[test]
    fn parses_host_header() {
        let h = parse_host_header(b"GET / HTTP/1.1\r\nHost: example.com:80\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(h, "example.com");
    }

    #[test]
    fn host_header_incomplete() {
        assert_eq!(parse_host_header(b"GET / HTTP/1.1\r\n"), Ok(None));
    }
}
