//! Ports the policy engine listens on inside the sandbox.
//!
//! Runtime configuration is no longer loaded from the environment and a
//! bind-mounted policy file. The engine runs in the worker process, so it is handed
//! a `NetworkPolicy` value directly — which removes the JSON policy file that had to
//! be written to disk for the container to read, along with the care needed to keep
//! that file out of a world-writable directory.

/// Explicit `CONNECT` proxy, for proxy-aware clients that dial it by name.
pub const PORT_CONNECT: u16 = 8080;
/// The transparent proxy port. All of the sandbox's TCP is redirected here, and the
/// handler classifies each connection (SNI/Host on any port) or falls back to the
/// `ip -> {domains}` map recorded by the resolver.
pub const PORT_TLS: u16 = 8443;
/// The policy-enforcing DNS resolver.
pub const PORT_DNS: u16 = 53;
