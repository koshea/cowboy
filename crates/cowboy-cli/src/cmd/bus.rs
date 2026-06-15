//! `cowboy message` / `cowboy inbox` — the daemon-mediated coordination bus.
//!
//! This is a structured message channel between sessions (and the user), not a
//! free chat: messages are stored in per-session inboxes by the daemon. A Ranch
//! coordinator routes richer events over the same bus in a later stage.

use anyhow::{bail, Context, Result};
use cowboy_core::daemonproto::{BusEvent, DaemonReq, DaemonResp, MsgTarget};

use crate::cmd::daemon;

/// `cowboy message <msg> --to <session> | --all`.
pub async fn send(message: String, to: Option<String>, all: bool) -> Result<()> {
    daemon::ensure_running().await?;
    let target = if all {
        MsgTarget::All
    } else {
        MsgTarget::Session(to.context("specify a target with --to <session> or --all")?)
    };
    let resp = daemon::request(DaemonReq::SendMessage {
        to: target,
        from: "user".into(),
        event: BusEvent::UserMessage(message),
    })
    .await
    .context("sending message via cowboyd")?;
    match resp {
        DaemonResp::Sent { delivered } => {
            println!("delivered to {delivered} session inbox(es)");
            Ok(())
        }
        DaemonResp::Err { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

/// `cowboy inbox [session]` — read (and drain) a session's inbox.
pub async fn inbox(session: Option<String>) -> Result<()> {
    let id = match session {
        Some(s) => s,
        None => {
            let root = crate::cmd::project_root()?;
            crate::session::latest_session_id(&root)
                .context("no session given and no recent session in this worktree")?
        }
    };
    daemon::ensure_running().await?;
    let resp = daemon::request(DaemonReq::GetInbox {
        session: id.clone(),
        drain: true,
    })
    .await
    .context("reading inbox via cowboyd")?;
    let messages = match resp {
        DaemonResp::Inbox { messages } => messages,
        DaemonResp::Err { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    };
    if messages.is_empty() {
        println!("inbox empty for {id}");
        return Ok(());
    }
    for m in &messages {
        println!("from {}: {}", m.from, describe(&m.event));
    }
    Ok(())
}

fn describe(e: &BusEvent) -> String {
    match e {
        BusEvent::UserMessage(s) | BusEvent::SessionMessage(s) => s.clone(),
        BusEvent::StatusUpdate(s) => format!("[status] {s}"),
        BusEvent::Blocked(s) => format!("[blocked] {s}"),
        BusEvent::AttentionRequested(s) => format!("[attention] {s}"),
        BusEvent::HandoffAvailable { artifact_id } => {
            format!("[handoff available] {artifact_id}")
        }
        BusEvent::ArtifactPublished { artifact_id, kind } => {
            format!("[artifact published] {artifact_id} ({kind})")
        }
    }
}
