//! `cowboy crew` — manage the Crew Roster (capability → model routing).
//!
//! The roster lives at `~/.config/cowboy/crew.yaml` (host-owned, like
//! `models.yaml`). The foreman (the selected `/model`) delegates work by
//! category + effort; Cowboy resolves that to a model here. A `<default>` slot
//! inherits the foreman. Quotas/rate-limits/spend belong to the gateway.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use cowboy_core::config::{ModelDef, ModelsConfig};
use cowboy_core::crew::{self, CrewConfig, Effort};

use crate::cli::CrewCommand;

pub async fn run(command: CrewCommand) -> Result<()> {
    match command {
        CrewCommand::Init { force } => init(force),
        CrewCommand::List => list(),
        CrewCommand::Show => show(),
        CrewCommand::Validate => validate(),
        CrewCommand::Usage => usage(),
        CrewCommand::Recommend => recommend(),
    }
}

/// The merged model catalogue (user + project), project overriding by name.
fn merged_models() -> Result<BTreeMap<String, ModelDef>> {
    let mut models = BTreeMap::new();
    if let Some(p) = ModelsConfig::user_path() {
        if let Some(c) = ModelsConfig::load_opt(&p)? {
            models.extend(c.models);
        }
    }
    if let Ok(root) = crate::cmd::project_root() {
        let proj = root.join(".cowboy").join("models.yaml");
        if let Some(c) = ModelsConfig::load_opt(&proj)? {
            models.extend(c.models);
        }
    }
    Ok(models)
}

fn model_names(models: &BTreeMap<String, ModelDef>) -> BTreeSet<String> {
    models.keys().cloned().collect()
}

/// Effective per-session model name override, consulted at model resolution:
/// `COWBOY_MODEL` (set by subagent routing) wins. Otherwise `None` → use the
/// models.yaml default, which IS the foreman (what `/model` selects). The crew
/// roster no longer overrides this — the foreman is always the selected model.
pub fn session_model_override() -> Option<String> {
    match std::env::var("COWBOY_MODEL") {
        Ok(m) if !m.is_empty() => Some(m),
        _ => None,
    }
}

/// The foreman model name for delegation routing: a routed `COWBOY_MODEL` if
/// this process is itself a worker, else the configured default model (what
/// `/model` selects). Used to fill `<default>` roster slots.
pub fn foreman_model() -> Option<String> {
    if let Ok(m) = std::env::var("COWBOY_MODEL") {
        if !m.is_empty() {
            return Some(m);
        }
    }
    default_model_name()
}

/// True when crew delegation is on (a roster exists and `delegation.enabled`).
pub fn crew_enabled() -> bool {
    matches!(crew::load(), Ok(Some(c)) if c.enabled())
}

/// Turn crew mode on/off (toggled from the `/model` picker). Enabling with no
/// roster yet bootstraps a default one from the model price tiers; disabling
/// just flips the flag and preserves the roster.
pub fn set_crew_enabled(enabled: bool) -> Result<()> {
    match crew::load()? {
        Some(mut cfg) => {
            cfg.delegation.enabled = enabled;
            crew::save(&cfg)
        }
        None if enabled => {
            // Bootstrap a default roster so "Crew" has something to route to.
            let models = merged_models()?;
            if models.is_empty() {
                bail!("no models configured; run `cowboy models setup` first");
            }
            let ranked = price_sorted(&models);
            let cheap = ranked.first().cloned().unwrap();
            let premium = ranked.last().cloned().unwrap();
            let standard = ranked
                .get(ranked.len() / 2)
                .cloned()
                .unwrap_or_else(|| cheap.clone());
            let mut cfg = crew::default_with_tiers(&cheap, &standard, &premium);
            cfg.delegation.enabled = true;
            crew::save(&cfg)
        }
        None => Ok(()), // already solo
    }
    .map_err(Into::into)
}

/// The configured default model name (project default, else user default) —
/// this is the foreman when no `COWBOY_MODEL` route is set.
fn default_model_name() -> Option<String> {
    if let Ok(root) = crate::cmd::project_root() {
        let proj = root.join(".cowboy").join("models.yaml");
        if let Some(d) = ModelsConfig::load_opt(&proj)
            .ok()
            .flatten()
            .and_then(|p| p.default)
        {
            return Some(d);
        }
    }
    ModelsConfig::user_path()
        .and_then(|p| ModelsConfig::load_opt(&p).ok().flatten())
        .and_then(|u| u.default)
}

/// Per-task-type temperature override for this session, set by the crew router
/// (`COWBOY_TEMPERATURE`). `None` → the model keeps its configured temperature.
pub fn temperature_override() -> Option<f32> {
    std::env::var("COWBOY_TEMPERATURE")
        .ok()
        .and_then(|t| t.parse::<f32>().ok())
}

/// Rank models cheapest→priciest by known total $/Mtok; unknown-priced last.
fn price_sorted(models: &BTreeMap<String, ModelDef>) -> Vec<String> {
    let mut v: Vec<(&String, f64, bool)> = models
        .iter()
        .map(|(name, d)| {
            let known = d.input_cost_per_mtok.is_some() || d.output_cost_per_mtok.is_some();
            let cost = d.input_cost_per_mtok.unwrap_or(0.0) + d.output_cost_per_mtok.unwrap_or(0.0);
            (name, cost, known)
        })
        .collect();
    // Known prices first (ascending), then unknown-priced by name.
    v.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.cmp(b.0))
    });
    v.into_iter().map(|(n, _, _)| n.clone()).collect()
}

fn init(force: bool) -> Result<()> {
    let path = crew::path().context("cannot resolve home config dir")?;
    if path.exists() && !force {
        bail!(
            "crew.yaml already exists at {} (use --force to overwrite)",
            path.display()
        );
    }
    let models = merged_models()?;
    if models.is_empty() {
        bail!("no models configured; run `cowboy models setup` first");
    }
    let ranked = price_sorted(&models);
    // Three tiers: cheapest, middle, priciest (reused when fewer than 3 models).
    let cheap = ranked.first().cloned().unwrap();
    let premium = ranked.last().cloned().unwrap();
    let standard = ranked
        .get(ranked.len() / 2)
        .cloned()
        .unwrap_or_else(|| cheap.clone());

    let cfg = crew::default_with_tiers(&cheap, &standard, &premium);
    crew::save(&cfg)?;
    println!("✓ wrote crew roster to {}", path.display());
    println!("  tiers: cheap={cheap}  standard={standard}  premium={premium}");
    println!(
        "  foreman: your selected model ({}) — change it with `cowboy models use` or /model",
        foreman_model().as_deref().unwrap_or("none set")
    );
    println!("  edit it, or review with `cowboy crew list`.");
    Ok(())
}

fn load_or_explain() -> Result<CrewConfig> {
    match crew::load()? {
        Some(c) => Ok(c),
        None => bail!("no crew roster yet — create one with `cowboy crew init`"),
    }
}

/// Shorten a model id to its last path segment for a readable grid
/// (e.g. `accounts/fireworks/models/kimi-k2p7-code` → `kimi-k2p7-code`).
fn short_model(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn list() -> Result<()> {
    let cfg = load_or_explain()?;
    let foreman = foreman_model().unwrap_or_else(|| "<default>".to_string());
    println!(
        "foreman: {}   crew: {}",
        foreman,
        if cfg.enabled() { "on" } else { "off (solo)" }
    );
    println!("(<default> slots inherit the foreman)");
    println!();
    // Size columns to the widest shortened model name so long ids stay readable.
    let cat_w = cfg
        .crew
        .keys()
        .map(|c| c.len())
        .chain(std::iter::once("CATEGORY".len()))
        .max()
        .unwrap_or(8)
        + 2;
    let mut col_w = Effort::all()
        .iter()
        .map(|e| e.as_str().len())
        .max()
        .unwrap_or(6);
    for cat in cfg.crew.keys() {
        for (_, model) in cfg.expanded(cat, &foreman) {
            col_w = col_w.max(short_model(&model).len());
        }
    }
    col_w += 2;

    print!("{:<cat_w$}", "CATEGORY");
    for e in Effort::all() {
        print!("{:<col_w$}", e.as_str());
    }
    println!();
    for cat in cfg.crew.keys() {
        print!("{cat:<cat_w$}");
        for (_, model) in cfg.expanded(cat, &foreman) {
            print!("{:<col_w$}", short_model(&model));
        }
        println!();
    }
    Ok(())
}

fn show() -> Result<()> {
    let cfg = load_or_explain()?;
    let yaml = serde_yaml_ng::to_string(&cfg).context("serializing crew config")?;
    print!("{yaml}");
    Ok(())
}

fn validate() -> Result<()> {
    let cfg = load_or_explain()?;
    let models = merged_models()?;
    let names = model_names(&models);
    match cfg.validate(&names) {
        Ok(()) => {
            println!("✓ crew roster is valid ({} categories)", cfg.crew.len());
            Ok(())
        }
        Err(e) => bail!("crew roster invalid: {e}"),
    }
}

fn usage() -> Result<()> {
    let rows = crew::usage_by_model(&crew::load_history());
    if rows.is_empty() {
        println!("no recorded crew activity yet (delegations are logged as they run)");
        return Ok(());
    }
    println!(
        "{:<16} {:>6} {:>9} {:>12}",
        "MODEL", "TASKS", "SUCCESS", "AVG"
    );
    for r in rows {
        let avg = r.avg_duration_ms();
        let avg_s = if avg >= 1000 {
            format!("{:.1}s", avg as f64 / 1000.0)
        } else {
            format!("{avg}ms")
        };
        println!(
            "{:<16} {:>6} {:>8}% {:>12}",
            r.model,
            r.tasks,
            r.success_pct(),
            avg_s
        );
    }
    Ok(())
}

fn recommend() -> Result<()> {
    let outcomes = crew::load_history();
    let recs = crew::recommend(&outcomes);
    if recs.is_empty() {
        println!(
            "no changes suggested ({} outcomes analyzed).",
            outcomes.len()
        );
        return Ok(());
    }
    println!("crew recommendations (review, then edit the roster yourself):");
    for r in recs {
        println!("  • {r}");
    }
    Ok(())
}
