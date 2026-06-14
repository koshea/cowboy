//! `cowboyd` — the cowboy coordination daemon. Tracks sessions and worktree
//! leases, prevents same-worktree collisions, and supervises session workers.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("COWBOY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    cowboy_cli::cmd::daemon::serve().await
}
