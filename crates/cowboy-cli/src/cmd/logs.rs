//! `cowboy logs` and `cowboy replay <id>` — session listing and replay.

use anyhow::Result;

use crate::session::replay as replay_mod;

pub async fn run() -> Result<()> {
    let root = crate::cmd::project_root()?;
    replay_mod::list(&root)
}

pub async fn replay(session_id: String) -> Result<()> {
    let root = crate::cmd::project_root()?;
    replay_mod::replay(&root, &session_id)
}
