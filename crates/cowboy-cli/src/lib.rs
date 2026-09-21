//! `cowboy-cli` library: the modules behind the `cowboy` client and the
//! `cowboyd` daemon binaries (both live in this crate so they share the agent
//! loop, sandbox, session, and daemon code).

pub mod agent;
pub mod banner;
pub mod cli;
pub mod cmd;
pub mod localsock;
pub mod mcp;
pub mod net;
pub mod project;
pub mod prompt;
pub mod sandbox;
pub mod session;
pub mod style;
pub mod tips;
pub mod ui;

/// Marker error for "I already told the user what is wrong".
///
/// Some failures are reports, not messages: `cowboy`'s first-run check prints a block
/// naming every gap and its remedy, and anyhow's `Error: …` line on top of that is
/// noise at best and a second, emptier explanation at worst. `main` recognises this and
/// exits non-zero in silence.
#[derive(Debug)]
pub struct AlreadyReported;

impl std::fmt::Display for AlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never rendered by `main`, but a stray `{}` somewhere should not print nothing
        // at all and leave a user with a bare exit code.
        write!(f, "cannot continue (see the report above)")
    }
}

impl std::error::Error for AlreadyReported {}
