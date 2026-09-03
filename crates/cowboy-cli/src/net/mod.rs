//! Host-side network policy state and git worktrees.
//!
//! Two things used to live here that no longer do. The **gateway control socket**
//! went when the policy engine moved into the worker process, so an `ask` is a
//! channel send rather than an authenticated TCP round trip. **Docker orchestration**
//! went with the container: the sandbox is built from kernel primitives, so there is
//! no daemon to talk to, no image to reconcile, and no bridge network to name.
//!
//! What remains is genuinely about the network *policy* (persisted approvals) plus
//! worktree management, which shares nothing with either.

pub mod approvals;
pub mod worktree;
