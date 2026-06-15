//! `cowboy crew` — manage the Crew Roster (capability → model routing).
//!
//! The roster lives at `~/.config/cowboy/crew.yaml` (host-owned, like
//! `models.yaml`). The planner delegates work by category + effort; Cowboy
//! resolves that to a model here. Quotas/rate-limits/spend belong to the gateway.

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
/// `COWBOY_MODEL` (set by subagent routing) wins; otherwise the crew planner
/// model, if a roster is configured. `None` → use the models.yaml default.
pub fn session_model_override() -> Option<String> {
    if let Ok(m) = std::env::var("COWBOY_MODEL") {
        if !m.is_empty() {
            return Some(m);
        }
    }
    match crew::load() {
        Ok(Some(c)) => Some(c.planner.model),
        _ => None,
    }
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
    println!("  planner: {premium}");
    println!("  edit it, or review with `cowboy crew list`.");
    Ok(())
}

fn load_or_explain() -> Result<CrewConfig> {
    match crew::load()? {
        Some(c) => Ok(c),
        None => bail!("no crew roster yet — create one with `cowboy crew init`"),
    }
}

fn list() -> Result<()> {
    let cfg = load_or_explain()?;
    println!("planner: {}", cfg.planner.model);
    println!();
    // Header
    print!("{:<14}", "CATEGORY");
    for e in Effort::all() {
        print!("{:<10}", e.as_str());
    }
    println!();
    // One row per category, ramps expanded to concrete models.
    for cat in cfg.crew.keys() {
        print!("{cat:<14}");
        for (_, model) in cfg.expanded(cat) {
            print!("{model:<10}");
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
