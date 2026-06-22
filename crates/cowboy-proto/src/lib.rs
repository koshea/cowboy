//! Wire protocol types shared across the cowboy stack — the `cowboyd` daemon
//! control API, the worker↔client session protocol, and the host↔gateway control
//! channel.
//!
//! This crate is intentionally dependency-light (just `serde`/`serde_json` +
//! `std`) so it compiles to **both** native targets and `wasm32` — the Yew web
//! client reuses these exact types, so the browser and the worker can never
//! drift on the wire format. `cowboy-core` re-exports these modules, so existing
//! `cowboy_core::{daemonproto,netproto}` paths keep working unchanged.

pub mod daemonproto;
pub mod netproto;
