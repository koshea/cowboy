//! The interactive/one-shot session engine: wire the model client, agent
//! runtime, and a UI (ratatui TUI on a terminal, console otherwise) into the
//! agent loop.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use cowboy_core::config::{AgentConfig, ConfigPaths, ModelsConfig, SecurityConfig};
use cowboy_core::model::OpenAiClient;
use tokio_util::sync::CancellationToken;

use crate::agent::tui::{run_event_loop, TuiUi, UiEvent};
use crate::agent::{AgentLoop, ConsoleUi};
use crate::net::docker::CliDocker;
use crate::net::runtime::AgentRuntime;

pub async fn run(task: Option<String>, _one_shot: bool) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);

    let security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    let agent_cfg = AgentConfig::load(&paths.agent).unwrap_or_default();
    let models = ModelsConfig::load(&paths.models)
        .context("loading .cowboy/models.yaml (run `cowboy init` first)")?;
    let profile = models.resolve(None)?;

    let model = OpenAiClient::from_profile(profile).context("building model client")?;
    let logger = crate::session::SessionLogger::create(&root).ok();
    if let Some(l) = &logger {
        eprintln!("session: {}", l.id());
    }
    let runtime = AgentRuntime::new(Box::new(CliDocker::new()), root, security);

    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });

    let task = resolve_task(task)?;
    let Some(task) = task else {
        println!("nothing to do.");
        return Ok(());
    };

    let behavior = agent_cfg.agent;
    if std::io::stdout().is_terminal() {
        run_tui(task, Box::new(model), runtime, behavior, cancel, logger)
    } else {
        let mut ui = ConsoleUi::new();
        let mut agent =
            AgentLoop::new(Box::new(model), runtime, behavior, cancel, &mut ui).with_logger(logger);
        agent.run(&task).await?;
        Ok(())
    }
}

/// Resolve the task: use the provided one, or prompt for it.
fn resolve_task(task: Option<String>) -> Result<Option<String>> {
    if let Some(t) = task {
        return Ok(Some(t));
    }
    use std::io::Write;
    print!("cowboy› what should I work on?\n> ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let t = line.trim().to_string();
    Ok((!t.is_empty()).then_some(t))
}

/// Run the ratatui front-end: the agent loop runs on a dedicated thread with
/// its own runtime; the main thread owns the terminal event loop.
#[allow(clippy::too_many_arguments)]
fn run_tui(
    task: String,
    model: Box<dyn cowboy_core::model::ModelClient>,
    runtime: AgentRuntime,
    behavior: cowboy_core::config::AgentBehavior,
    cancel: CancellationToken,
    logger: Option<crate::session::SessionLogger>,
) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<UiEvent>();
    let done_tx = tx.clone();
    let agent_cancel = cancel.clone();
    let task_for_agent = task.clone();

    let handle = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = done_tx.send(UiEvent::Notice(format!("runtime error: {e}")));
                let _ = done_tx.send(UiEvent::Done);
                return;
            }
        };
        let mut ui = TuiUi { tx };
        let result = {
            let mut agent =
                AgentLoop::new(model, runtime, behavior, agent_cancel, &mut ui).with_logger(logger);
            rt.block_on(agent.run(&task_for_agent))
        };
        if let Err(e) = result {
            let _ = ui.tx.send(UiEvent::Notice(format!("error: {e}")));
        }
        let _ = ui.tx.send(UiEvent::Done);
    });

    run_event_loop("cowboy", &task, rx, cancel)?;
    let _ = handle.join();
    Ok(())
}
