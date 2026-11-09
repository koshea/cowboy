//! `cowboy crew` — manage the Crew Roster (capability → model routing).
//!
//! The roster lives at `~/.config/cowboy/crew.yaml` (host-owned, like
//! `models.yaml`). The foreman (the selected `/model`) delegates work by
//! category + effort; Cowboy resolves that to a model here. A `<default>` slot
//! inherits the foreman. Quotas/rate-limits/spend belong to the gateway.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use cowboy_core::config::{ModelDef, ModelRef, ModelsConfig};
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

/// The merged model catalogue (user + project) across every provider, project
/// overriding by `provider/name`.
fn merged_models() -> Result<BTreeMap<ModelRef, ModelDef>> {
    let user = match ModelsConfig::user_path() {
        Some(p) => ModelsConfig::load_opt(&p)?,
        None => None,
    };
    let project = match crate::cmd::project_root() {
        Ok(root) => ModelsConfig::load_opt(&root.join(".cowboy").join("models.yaml"))?,
        Err(_) => None,
    };
    Ok(cowboy_core::config::merged_models(
        user.as_ref(),
        project.as_ref(),
    ))
}

fn model_names(models: &BTreeMap<ModelRef, ModelDef>) -> BTreeSet<String> {
    models.keys().map(ToString::to_string).collect()
}

/// Effective per-session model override, consulted at model resolution:
/// `COWBOY_MODEL` (set by subagent routing, always `provider/name`) wins.
/// Otherwise `None` → use the models.yaml default, which IS the foreman (what
/// `/model` selects). A value that is not `provider/name` is treated like any
/// other stale override: resolution falls back to the default.
pub fn session_model_override() -> Option<ModelRef> {
    match std::env::var("COWBOY_MODEL") {
        Ok(m) if !m.is_empty() => ModelRef::parse(&m).ok(),
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
            return Some(d.to_string());
        }
    }
    ModelsConfig::user_path()
        .and_then(|p| ModelsConfig::load_opt(&p).ok().flatten())
        .and_then(|u| u.default)
        .map(|d| d.to_string())
}

/// Per-task-type temperature override for this session, set by the crew router
/// (`COWBOY_TEMPERATURE`). `None` → the model keeps its configured temperature.
pub fn temperature_override() -> Option<f32> {
    std::env::var("COWBOY_TEMPERATURE")
        .ok()
        .and_then(|t| t.parse::<f32>().ok())
}

/// Rank models (across every provider) cheapest→priciest by known total $/Mtok;
/// unknown-priced last. Returns qualified `provider/name`s.
fn price_sorted(models: &BTreeMap<ModelRef, ModelDef>) -> Vec<String> {
    let mut v: Vec<(&ModelRef, f64, bool)> = models
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
    v.into_iter().map(|(n, _, _)| n.to_string()).collect()
}

fn init(force: bool) -> Result<()> {
    let path = crew::path().context("cannot resolve home config dir")?;
    if path.exists() {
        if !force {
            bail!(
                "crew.yaml already exists at {} (use --force to overwrite)",
                path.display()
            );
        }
        // The roster is hand-tuned in practice — which model each role gets, and the
        // delegation grants — so regenerating it from price tiers discards choices the
        // template cannot reconstruct.
        crate::ui::warn(&format!(
            "--force will replace your hand-edited roster at {}",
            path.display()
        ));
        if !crate::prompt::confirm_destructive("Overwrite crew.yaml?")? {
            return Ok(());
        }
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
    crate::ui::ok(&format!("wrote crew roster to {}", path.display()));
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
    // Size columns to the widest slot. Slots are `provider/name` (or a harness), so
    // the provider stays visible — with several, it is the point of the grid.
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
            col_w = col_w.max(model.len());
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
            print!("{model:<col_w$}");
        }
        println!();
    }
    // Print the meaning the foreman is actually routing on. Without this the
    // roster shows *where* work goes but not *what* each slot claims to be for, and
    // the user's idea of a category can drift from the model's for a long time
    // before anything looks wrong.
    println!();
    println!("what each category means to the agent:");
    for cat in cfg.crew.keys() {
        match cfg.description_for(cat) {
            Some(d) => println!("  {cat:<cat_w$}{d}"),
            None => println!(
                "  {cat:<cat_w$}(undefined — the agent will guess from the name; \
                 set `descriptions.{cat}` in crew.yaml)"
            ),
        }
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
    let mut names = model_names(&models);
    // A slot may name a harness (harnesses.yaml) as well as a model — but never a
    // name that is both, which would make the route ambiguous.
    let harnesses =
        cowboy_core::harness::HarnessesConfig::load_user().context("loading harnesses.yaml")?;
    let clash = cowboy_core::harness::colliding_names(names.iter().map(String::as_str), &harnesses);
    if !clash.is_empty() {
        bail!(
            "crew roster invalid: {} defined in both models.yaml and harnesses.yaml — \
             rename one so a crew slot is unambiguous",
            clash.join(", ")
        );
    }
    names.extend(harnesses.names().map(str::to_string));
    // Each provider a roster routes to must exist (and, for a ChatGPT sign-in, be
    // signed in) — otherwise every subagent sent there fails at its first turn.
    let providers =
        cowboy_core::config::ProvidersConfig::load_global().context("loading providers.yaml")?;
    let problems = provider_problems(&cfg, &providers);
    if !problems.is_empty() {
        bail!("crew roster invalid: {}", problems.join("; "));
    }
    match cfg.validate(&names) {
        Ok(()) => {
            crate::ui::ok(&format!(
                "crew roster is valid ({} categories)",
                cfg.crew.len()
            ));
            Ok(())
        }
        Err(e) => bail!("crew roster invalid: {e}"),
    }
}

/// Every provider-level reason the roster's model slots cannot run, one line each:
/// a provider missing from `providers.yaml`, or a ChatGPT provider nobody has
/// signed in to. Harness and `<default>` slots carry no provider and are skipped.
pub fn provider_problems(
    cfg: &CrewConfig,
    providers: &cowboy_core::config::ProvidersConfig,
) -> Vec<String> {
    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (cat, ramp) in &cfg.crew {
        for slot in ramp.models() {
            if let Ok(r) = ModelRef::parse(slot) {
                used.entry(r.provider).or_default().insert(cat.clone());
            }
        }
    }
    let mut out = Vec::new();
    for (provider, cats) in used {
        let cats = cats.into_iter().collect::<Vec<_>>().join(", ");
        match providers.providers.get(&provider) {
            None => out.push(format!(
                "provider `{provider}` (used by {cats}) is not in providers.yaml"
            )),
            Some(p) => {
                if let Some(why) = sign_in_problem(&provider, p) {
                    out.push(format!("provider `{provider}` (used by {cats}) {why}"));
                }
            }
        }
    }
    out
}

/// Why a configured provider cannot authenticate right now, if it cannot.
fn sign_in_problem(_name: &str, _provider: &cowboy_core::config::Provider) -> Option<String> {
    None
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

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::{Provider, ProvidersConfig};

    #[test]
    fn provider_problems_names_each_missing_provider_and_its_categories() {
        let cfg: CrewConfig = serde_yaml_ng::from_str(
            "crew:\n  general: bifrost/glm\n  deep: chatgpt/gpt\n  review: codex\n  \
             tests: <default>\n",
        )
        .unwrap();
        let mut providers = ProvidersConfig::default();
        providers
            .providers
            .insert("bifrost".into(), Provider::api_key("http://x/v1", ""));
        let problems = provider_problems(&cfg, &providers);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("`chatgpt`") && problems[0].contains("deep"),
            "{problems:?}"
        );

        providers
            .providers
            .insert("chatgpt".into(), Provider::api_key("http://y/v1", "k"));
        assert!(provider_problems(&cfg, &providers).is_empty());
    }
}
