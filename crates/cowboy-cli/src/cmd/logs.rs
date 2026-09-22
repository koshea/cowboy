//! `cowboy logs` and `cowboy replay <id>` — session listing and replay.

use anyhow::Result;

use crate::agent::tui::SessionCtx;
use crate::session::replay as replay_mod;

pub async fn run() -> Result<()> {
    let root = crate::cmd::project_root()?;
    replay_mod::list(&root)
}

pub async fn replay(session_id: String, tui: bool) -> Result<()> {
    let root = crate::cmd::project_root()?;
    if !tui {
        return replay_mod::replay(&root, &session_id);
    }

    let id = replay_mod::resolve_id(&root, &session_id)?;
    let journal = crate::session::session_dir(&root, &id).join("events.jsonl");
    let ctx = SessionCtx {
        root: root.clone(),
        models: Vec::new(),
        current_model: String::new(),
        ranch_id: None,
        workstream_id: None,
        suggestions: Vec::new(),
    };
    crate::cmd::attach::replay_journal(
        &journal,
        &format!("{} · {id}", root.display()),
        "saved",
        ctx,
    )
}
