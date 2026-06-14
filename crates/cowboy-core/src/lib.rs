//! Core types shared by the cowboy host process: configuration, model client,
//! network policy, and error types.

pub mod config;
pub mod error;
pub mod model;
pub mod netproto;
pub mod policy;

pub use error::{Error, Result};
