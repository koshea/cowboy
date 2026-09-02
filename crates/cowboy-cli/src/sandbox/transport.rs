//! The egress seam: how the sandbox's outbound traffic is forced to the policy
//! engine.
//!
//! Separate from [`super::Sandbox`] on purpose. Filesystem confinement and
//! network interception fail for different reasons and on different kernel
//! features, and which transport a host can run is the question most likely to
//! differ between machines — so it is the seam that actually earns its keep.
//!
//! **Containment is not this trait's job.** The session network namespace holds
//! no host-connected device, so a transport that fails to install (or is torn
//! down) leaves the sandbox with *no* egress rather than unrestricted egress. A
//! transport provides **transparency**: it makes the agent's traffic visible to
//! the policy engine instead of merely unroutable. That inversion is why this can
//! be pluggable at all — see `docs/src/security/sandbox-decisions.md`.

use anyhow::Result;

/// A kernel feature a transport needs, for `cowboy doctor` to report.
///
/// The distinction between "present but not loaded" and "unavailable" matters:
/// the first is fixable at runtime, the second needs a kernel rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelRequirement {
    /// Module name as it appears in `/proc/modules`, e.g. `nft_nat`.
    pub module: &'static str,
    /// Kernel config symbol, e.g. `CONFIG_NFT_NAT`.
    pub config: &'static str,
    /// Why this transport needs it, shown to the user when it is missing.
    pub reason: &'static str,
    /// Whether the kernel can autoload it on demand from a user namespace.
    ///
    /// nf_tables expression modules can (nf_tables calls `request_module()` from
    /// a privileged kernel context); most modules cannot, because `init_module`
    /// requires `CAP_SYS_MODULE` in the initial user namespace.
    pub autoloads: bool,
}

/// Forces the sandbox's outbound traffic through the host-side policy engine.
///
/// Installed during session bring-up **while `CAP_NET_ADMIN` is still held** in
/// the sandbox user namespace, then never again — every agent process runs with
/// an empty capability bounding set, so it cannot alter or remove enforcement.
#[async_trait::async_trait]
pub trait EgressTransport: Send + Sync {
    /// Short name for logs and `doctor` output, e.g. `nft-dnat`.
    fn name(&self) -> &'static str;

    /// Kernel features this transport needs.
    fn requirements(&self) -> Vec<KernelRequirement>;

    /// Install interception inside the already-entered sandbox network namespace.
    ///
    /// The caller guarantees: we are in the session's user + network namespace,
    /// `CAP_NET_ADMIN` is held, and no agent process exists yet. Must be
    /// idempotent — a session that re-enters bring-up must not end up with
    /// duplicated rules.
    async fn install(&self, cfg: &TransportConfig) -> Result<()>;

    /// Confirm interception is actually live, from inside the sandbox network
    /// namespace. Callers **must not** run agent commands when this errors: a
    /// transport that reports success while enforcing nothing would make the
    /// traffic unroutable rather than policed, which is safe but silently broken.
    async fn verify(&self) -> Result<()>;
}

/// Where a transport should send intercepted traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportConfig {
    /// Loopback port inside the sandbox where the relay accepts intercepted TCP.
    pub relay_port: u16,
    /// Loopback port inside the sandbox where the relay accepts DNS queries.
    pub dns_port: u16,
    /// Address assigned to the black-hole device, which exists only so the
    /// routing decision succeeds and the nat hook can fire — a loopback-only
    /// namespace fails `connect()` with `ENETUNREACH` before nftables sees the
    /// packet. It is deliberately connected to nothing.
    pub sandbox_cidr: String,
    /// Next hop for the default route. No such host exists; nothing may reach it.
    pub blackhole_gateway: String,
}
