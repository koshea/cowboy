//! Host-side networking: Docker orchestration, the gateway control socket, and
//! Docker Compose detection.
//!
//! Orchestration and the control socket land in Slice C. Compose detection is
//! used by `doctor`/`init` to offer network approval.

pub mod approvals;
pub mod compose;
pub mod control;
pub mod docker;
pub mod gateway;
pub mod runtime;
pub mod worktree;
