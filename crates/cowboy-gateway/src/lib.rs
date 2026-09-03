//! The network policy engine: what the agent is allowed to reach, and why.
//!
//! This used to be a **separate binary in its own container**, talking to the host
//! over a token-authenticated TCP channel so it could ask a human about an `ask`
//! verdict. That channel existed only because the policy engine was on the wrong
//! side of a boundary: it ran inside the sandbox, where it could not be trusted with
//! the decision, so every decision had to be shipped out and a verdict shipped back.
//!
//! Now it is a library the **worker** links in and runs host-side, on the trusted
//! side of the boundary. An `ask` is a function call to [`Approver`] rather than a
//! network round trip, so the authenticated listener, its per-session token, and the
//! policy file written out for the container are all simply gone — deleted rather
//! than ported.
//!
//! What crosses the boundary instead is much smaller: the relay inside the sandbox
//! reports a connection's original destination over an anonymous socketpair, and
//! receives either a refusal or an already-connected file descriptor. See
//! `docs/src/security/model.md` for why that channel is the enforcement boundary.

pub mod dns;
pub mod dns_policy;
pub mod http;
pub mod sni;
pub mod state;

use cowboy_core::netproto::{NetworkAttempt, Verdict};

/// Decides `ask` verdicts and records decisions.
///
/// Implemented host-side by the worker, which routes a question to whichever UI is
/// attached and returns the answer. Replaces the TCP control client entirely.
///
/// **Must fail closed.** With no one to ask — a non-interactive run, or a detached
/// session with no client — the answer is `Deny`. An implementation that returned
/// `Allow` on timeout would silently undo the whole policy.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// Ask whether this attempt should be allowed. `reason` explains why we are
    /// asking (for example that a DNS query looks like tunnelling), so the prompt
    /// can say something more useful than the destination alone.
    async fn ask(&self, attempt: &NetworkAttempt, reason: Option<&str>) -> Verdict;

    /// Report a decision the policy made on its own, for logging and the activity
    /// view. Never blocks a connection.
    async fn event(&self, attempt: &NetworkAttempt, verdict: Verdict, reason: String);
}

/// An [`Approver`] that denies everything, for non-interactive runs.
///
/// The explicit form of the fail-closed default, so a caller with no UI attached
/// has something honest to pass rather than being tempted to skip the check.
pub struct DenyAll;

#[async_trait::async_trait]
impl Approver for DenyAll {
    async fn ask(&self, _attempt: &NetworkAttempt, _reason: Option<&str>) -> Verdict {
        Verdict::Deny
    }
    async fn event(&self, _attempt: &NetworkAttempt, _verdict: Verdict, _reason: String) {}
}
