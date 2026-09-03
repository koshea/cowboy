//! `cowboy-cli` library: the modules behind the `cowboy` client and the
//! `cowboyd` daemon binaries (both live in this crate so they share the agent
//! loop, sandbox, session, and daemon code).

pub mod agent;
pub mod cli;
pub mod cmd;
pub mod mcp;
pub mod net;
pub mod project;
pub mod sandbox;
pub mod session;
pub mod style;
