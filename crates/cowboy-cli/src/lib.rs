//! `cowboy-cli` library: the modules behind the `cowboy` client and the
//! `cowboyd` daemon binaries (both live in this crate so they share the agent
//! loop, docker runtime, session, and daemon code).

pub mod agent;
pub mod cli;
pub mod cmd;
pub mod net;
pub mod session;
