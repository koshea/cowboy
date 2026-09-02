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

pub mod broker;
pub mod channel;
pub mod nft;
pub mod relay;

use anyhow::{Context, Result};

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

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            relay_port: crate::sandbox::RELAY_PORT,
            dns_port: crate::sandbox::DNS_PORT,
            // A link-local range: unroutable by definition, so if the black-hole
            // device were ever wired to something real it still could not carry
            // traffic off the machine.
            sandbox_cidr: "169.254.11.2/24".to_string(),
            blackhole_gateway: "169.254.11.1".to_string(),
        }
    }
}

/// The nftables transport: a black-hole device to make routing succeed, and a nat
/// hook that catches every packet before it reaches nowhere.
///
/// Chosen over a userspace TCP/IP stack on evidence — see
/// `docs/src/security/sandbox-decisions.md`. The alternative remains implementable
/// behind this trait, which is the seam's purpose.
pub struct NftTransport {
    cfg: TransportConfig,
}

impl NftTransport {
    pub fn new(cfg: TransportConfig) -> Self {
        Self { cfg }
    }

    /// Create the black-hole device and default route.
    ///
    /// A veth pair rather than a dummy device because `CONFIG_VETH` is loaded on the
    /// target host while `CONFIG_DUMMY` is not. Only one end is configured and the
    /// peer is left down and unconnected: the route exists so that `connect()` gets
    /// as far as the nat hook, and the packet is redirected to the relay before it
    /// can go anywhere. If the redirect were ever absent, the packet would reach a
    /// device attached to nothing — which is why a total transport failure means no
    /// egress rather than open egress.
    async fn setup_device(&self) -> Result<()> {
        // `route_localnet` lets the redirected packet be delivered to a loopback
        // address even though it arrived on a non-loopback route.
        run("sysctl", &["-qw", "net.ipv4.conf.all.route_localnet=1"]).await?;
        run(
            "ip",
            &[
                "link", "add", "cowboy0", "type", "veth", "peer", "name", "cowboy1",
            ],
        )
        .await?;
        run(
            "ip",
            &["addr", "add", &self.cfg.sandbox_cidr, "dev", "cowboy0"],
        )
        .await?;
        run("ip", &["link", "set", "cowboy0", "up"]).await?;
        // The peer must be up too, or the route is marked `linkdown` and may be
        // skipped by the routing decision — which would put us back to
        // `ENETUNREACH` before the nat hook ever runs.
        run("ip", &["link", "set", "cowboy1", "up"]).await?;
        run(
            "ip",
            &[
                "route",
                "add",
                "default",
                "via",
                &self.cfg.blackhole_gateway,
                "dev",
                "cowboy0",
            ],
        )
        .await?;
        Ok(())
    }
}

async fn run(bin: &str, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {bin} {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[async_trait::async_trait]
impl EgressTransport for NftTransport {
    fn name(&self) -> &'static str {
        "nft-dnat"
    }

    fn requirements(&self) -> Vec<KernelRequirement> {
        vec![
            KernelRequirement {
                module: "nf_tables",
                config: "CONFIG_NF_TABLES",
                reason: "the nftables ruleset that redirects sandbox traffic to the relay",
                autoloads: true,
            },
            KernelRequirement {
                module: "nf_nat",
                config: "CONFIG_NF_NAT",
                reason: "destination rewriting for intercepted connections",
                autoloads: true,
            },
            KernelRequirement {
                module: "nft_chain_nat",
                config: "CONFIG_NFT_CHAIN_NAT",
                reason: "the nat chain the redirect rules live in",
                autoloads: true,
            },
            KernelRequirement {
                module: "nf_conntrack",
                config: "CONFIG_NF_CONNTRACK",
                reason: "recovering each connection's original destination",
                autoloads: true,
            },
            KernelRequirement {
                module: "veth",
                config: "CONFIG_VETH",
                reason: "the black-hole device that makes the routing decision succeed",
                autoloads: true,
            },
        ]
    }

    async fn install(&self, cfg: &TransportConfig) -> Result<()> {
        self.setup_device()
            .await
            .context("creating the sandbox's black-hole device")?;
        nft::apply(cfg)
            .await
            .context("applying the interception ruleset")?;
        Ok(())
    }

    async fn verify(&self) -> Result<()> {
        if !nft::is_loaded().await {
            anyhow::bail!(
                "the interception ruleset is not loaded. Refusing to run: traffic would be \
                 unroutable rather than policed — safe, but silently broken."
            );
        }
        Ok(())
    }
}
