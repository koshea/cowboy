//! The sandbox *plan*: what confinement to apply, computed as a pure value.
//!
//! Deliberately side-effect free. Nothing here creates a namespace, mounts
//! anything, or touches the network — it turns host-owned configuration plus the
//! current grant set into a [`plan::SandboxPlan`] that the executor in
//! `cowboy-cli` then applies. Keeping the decision separate from the doing is what
//! makes the security-relevant logic (what is masked, what is refused, what is
//! writable) unit-testable without a daemon, a container, or root.
//!
//! Host facts the plan needs — does this path exist, where is the git common dir,
//! what does `~` expand to — arrive through the [`probe::HostProbe`] seam so tests
//! can describe a filesystem instead of building one.

pub mod denylist;
pub mod plan;
pub mod probe;

pub use denylist::{DenyReason, Denylist};
pub use plan::{
    Bind, BindMode, LandlockRules, ResourceLimits, SandboxPlan, SeccompProfile, SHIM_PATH,
};
pub use probe::HostProbe;
