//! Core types shared by the cowboy host process: configuration, model client,
//! network policy, and error types.

pub mod agents;
pub mod artifact;
pub mod config;
pub mod crew;
pub mod decision;
pub mod error;
pub mod lifecycle;
pub mod mcp;
pub mod memory;
pub mod model;
pub mod model_defaults;
pub mod policy;

// The wire protocol types live in `cowboy-proto` (dependency-light + wasm-safe so
// the Yew web client shares them). Re-exported here so existing
// `cowboy_core::{daemonproto,netproto}` paths keep working unchanged.
pub use cowboy_proto::{daemonproto, netproto};
pub mod ranch;
pub mod scope;
pub mod skills;
pub mod time;
pub mod tokens;
pub mod usersecrets;

pub use error::{Error, Result};
