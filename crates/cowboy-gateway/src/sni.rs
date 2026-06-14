//! Extract the SNI server name from a buffered TLS ClientHello without
//! terminating or decrypting the connection.
//!
//! We parse just enough of the first TLS record to read the `server_name`
//! extension, then the original bytes are spliced through untouched. Parsing
//! untrusted network input, so this is fuzzed/property-tested for panics.

use tls_parser::{
    parse_tls_extensions, parse_tls_plaintext, TlsExtension, TlsMessage, TlsMessageHandshake,
};

/// Outcome of attempting to read the SNI from buffered bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SniResult {
    /// SNI hostname recovered.
    Found(String),
    /// A complete ClientHello was parsed but carried no SNI (e.g. ECH/none).
    NoSni,
    /// Not enough bytes yet; the caller should read more and retry.
    Incomplete,
    /// The bytes are not a TLS ClientHello.
    NotTls,
}

/// Try to extract the SNI from the start of a TLS stream.
pub fn extract_sni(buf: &[u8]) -> SniResult {
    // A TLS record header is 5 bytes; the handshake needs more.
    if buf.len() < 5 {
        return SniResult::Incomplete;
    }
    // Record type 22 = handshake; otherwise this isn't a TLS handshake.
    if buf[0] != 0x16 {
        return SniResult::NotTls;
    }

    let record = match parse_tls_plaintext(buf) {
        Ok((_, record)) => record,
        // Incomplete record: need more bytes.
        Err(tls_parser::Err::Incomplete(_)) => return SniResult::Incomplete,
        Err(_) => return SniResult::NotTls,
    };

    for msg in &record.msg {
        let TlsMessage::Handshake(TlsMessageHandshake::ClientHello(hello)) = msg else {
            continue;
        };
        let Some(ext_bytes) = hello.ext else {
            return SniResult::NoSni;
        };
        let Ok((_, exts)) = parse_tls_extensions(ext_bytes) else {
            return SniResult::NoSni;
        };
        for ext in exts {
            if let TlsExtension::SNI(names) = ext {
                for (_sni_type, name) in names {
                    if let Ok(s) = std::str::from_utf8(name) {
                        if !s.is_empty() {
                            return SniResult::Found(s.to_string());
                        }
                    }
                }
                return SniResult::NoSni;
            }
        }
        return SniResult::NoSni;
    }

    SniResult::Incomplete
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal TLS record framing around a handshake body.
    fn tls_record(handshake: &[u8]) -> Vec<u8> {
        let mut v = vec![0x16, 0x03, 0x01];
        v.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        v.extend_from_slice(handshake);
        v
    }

    /// Construct a ClientHello carrying a single SNI extension for `host`.
    fn client_hello_with_sni(host: &str) -> Vec<u8> {
        // SNI extension body: server_name_list.
        let mut sni_entry = vec![0x00]; // name_type = host_name
        sni_entry.extend_from_slice(&(host.len() as u16).to_be_bytes());
        sni_entry.extend_from_slice(host.as_bytes());
        let mut sni_list = (sni_entry.len() as u16).to_be_bytes().to_vec();
        sni_list.extend_from_slice(&sni_entry);
        let mut ext = vec![0x00, 0x00]; // extension type 0 = server_name
        ext.extend_from_slice(&(sni_list.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sni_list);

        let mut exts = (ext.len() as u16).to_be_bytes().to_vec();
        exts.extend_from_slice(&ext);

        // ClientHello body.
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // client_version TLS 1.2
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0x00); // session_id length
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher suites
        body.extend_from_slice(&[0x01, 0x00]); // compression methods
        body.extend_from_slice(&exts);

        // Handshake header: type 1 (ClientHello) + 3-byte length.
        let mut hs = vec![0x01];
        let len = body.len();
        hs.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        hs.extend_from_slice(&body);
        hs
    }

    #[test]
    fn extracts_sni() {
        let bytes = tls_record(&client_hello_with_sni("api.github.com"));
        assert_eq!(
            extract_sni(&bytes),
            SniResult::Found("api.github.com".to_string())
        );
    }

    #[test]
    fn incomplete_when_short() {
        assert_eq!(extract_sni(&[0x16, 0x03]), SniResult::Incomplete);
    }

    #[test]
    fn not_tls_for_http() {
        assert_eq!(extract_sni(b"GET / HTTP/1.1\r\n\r\n"), SniResult::NotTls);
    }

    #[test]
    fn truncated_record_is_incomplete() {
        let mut bytes = tls_record(&client_hello_with_sni("example.com"));
        bytes.truncate(10);
        assert_eq!(extract_sni(&bytes), SniResult::Incomplete);
    }
}
