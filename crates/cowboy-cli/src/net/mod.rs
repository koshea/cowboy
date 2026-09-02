//! Host-side networking: Docker orchestration and Compose detection.
//!
//! The gateway control socket used to live here. It is gone: the policy engine now
//! runs in the worker process, so an `ask` is a channel send rather than an
//! authenticated TCP round trip. See `crate::sandbox::policy`.

pub mod approvals;
pub mod compose;
pub mod docker;
pub mod gateway;
pub mod runtime;
pub mod worktree;
