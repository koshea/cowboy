//! `cowboy harnesses` — the external agent CLIs the crew can delegate to
//! (`~/.config/cowboy/harnesses.yaml`), and whether each is ready to use.

use anyhow::{Context, Result};
use cowboy_core::harness::{AuthExposure, HarnessesConfig};

use crate::style;

pub fn run() -> Result<()> {
    let path = HarnessesConfig::user_path();
    let cfg = HarnessesConfig::load_user().context("loading harnesses.yaml")?;
    if cfg.harnesses.is_empty() {
        println!(
            "no harnesses configured. Add one to {}:\n\n  harnesses:\n    grok:\n      kind: grok\n\n\
             then route a crew category to it (`exploration: grok` in crew.yaml), or ask the \
             agent to \"have grok …\".",
            path.map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/cowboy/harnesses.yaml".into())
        );
        return Ok(());
    }
    for (name, def) in &cfg.harnesses {
        let i = crate::agent::harness::inspect(def);
        println!(
            "{}  ({} CLI{})",
            style::bold(name),
            def.kind.as_str(),
            def.model
                .as_deref()
                .map(|m| format!(", model {m}"))
                .unwrap_or_default()
        );
        match &i.binary {
            Ok(b) => println!(
                "  binary   {} {}",
                b.display(),
                style::dim(i.version.as_deref().unwrap_or("(version unknown)"))
            ),
            Err(e) => println!("  binary   {}", style::red(&format!("missing — {e}"))),
        }
        println!(
            "  login    {}",
            if i.logged_in {
                style::green("present")
            } else {
                style::red(&format!("missing — run `{} login`", def.kind.spec().binary))
            }
        );
        println!(
            "  exposes  {}",
            match def.auth {
                AuthExposure::AuthFile => "the login file only",
                AuthExposure::FullHome => "the login + vendor config (auth: full_home)",
            }
        );
        println!("  hosts    {}", def.hosts().join(", "));
        println!();
    }
    Ok(())
}
