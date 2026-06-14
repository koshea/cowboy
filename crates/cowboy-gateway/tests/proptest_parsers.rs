//! Fuzz-style property tests: the gateway parsers must never panic on arbitrary
//! (adversarial) bytes — they parse untrusted network input. We assert totality
//! (the function returns for any input) rather than a specific result.
//!
//! `cowboy-gateway` is a binary crate, so we re-include the parser modules here
//! to exercise them directly.

#[path = "../src/http.rs"]
mod http;
#[path = "../src/sni.rs"]
mod sni;

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn extract_sni_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = sni::extract_sni(&bytes);
    }

    #[test]
    fn parse_connect_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = http::parse_connect(&bytes);
    }

    #[test]
    fn parse_host_header_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = http::parse_host_header(&bytes);
    }

    /// Bytes that start like a TLS handshake but are otherwise random must still
    /// terminate without panic (exercises the tls-parser path more deeply).
    #[test]
    fn tls_prefixed_bytes_never_panic(tail in prop::collection::vec(any::<u8>(), 0..2048)) {
        let mut bytes = vec![0x16, 0x03, 0x01];
        bytes.extend_from_slice(&tail);
        let _ = sni::extract_sni(&bytes);
    }
}
