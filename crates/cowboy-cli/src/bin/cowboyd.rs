//! `cowboyd` — the cowboy coordination daemon. Tracks sessions and worktree
//! leases, prevents same-worktree collisions, and supervises session workers.

use anyhow::Result;
use clap::Parser;

/// The daemon takes no options — it is started automatically and configured through
/// the environment (`COWBOY_LOG`, `COWBOY_DAEMON_LINGER`, the XDG directories).
///
/// It is parsed anyway, for two reasons. `cowboyd` is version-locked to `cowboy`, so
/// "which daemon is actually running?" is a question worth being able to ask, and
/// argv used to be ignored entirely: `cowboyd --version` printed nothing and started
/// a daemon instead, which is a surprising way to answer a harmless question.
#[derive(Parser)]
#[command(
    name = "cowboyd",
    version,
    about = "The cowboy coordination daemon (started automatically by `cowboy`)",
    long_about = "Tracks sessions and worktree leases, prevents same-worktree \
                  collisions, and supervises session workers.\n\n\
                  You do not normally run this yourself: `cowboy` starts a matching \
                  daemon on demand and stops it once no sessions remain. It takes no \
                  options; behaviour comes from the environment (COWBOY_LOG, \
                  COWBOY_DAEMON_LINGER) and the XDG directories."
)]
struct Args {}

#[tokio::main]
async fn main() -> Result<()> {
    Args::parse();

    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("COWBOY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    cowboy_cli::cmd::daemon::serve().await
}
