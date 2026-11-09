//! `cowboy crew` — manage the crews (capability → model routing).
//!
//! The crews live in `~/.config/cowboy/crew.yaml` (host-owned, like
//! `models.yaml`): named rosters, which one new sessions start with (`active`),
//! and the delegation settings they share. The foreman (the selected `/model`,
//! or the crew's own `foreman:`) delegates work by category + effort; Cowboy
//! resolves that to a model here. A `<default>` slot inherits the foreman.
//! Quotas/rate-limits/spend belong to the gateway.
//!
//! A session runs one crew. It starts with `COWBOY_CREW` (set for subagents, so
//! they route with their parent's crew) or the file's `active`, and `/crew <name>`
//! switches it for that session alone.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::RwLock;

use anyhow::{bail, Context, Result};
use cowboy_core::config::{ModelDef, ModelRef, ModelsConfig};
use cowboy_core::crew::{self, CrewConfig, CrewDef, CrewFile, Effort};
use cowboy_core::crew_text;

use crate::cli::CrewCommand;

/// The environment variable a session's crew is passed down in.
pub const ENV_CREW: &str = "COWBOY_CREW";

pub async fn run(command: CrewCommand) -> Result<()> {
    match command {
        CrewCommand::Init { force } => init(force),
        CrewCommand::List { crew } => list(crew.as_deref()),
        CrewCommand::Show => show(),
        CrewCommand::Validate => validate(),
        CrewCommand::Use { name } => use_crew(&name),
        CrewCommand::Solo => solo(),
        CrewCommand::New {
            name,
            preset: Some(crate::cli::CrewPreset::Chatgpt),
            provider,
            ..
        } => add_chatgpt_crew(&name, provider.as_deref()).await,
        CrewCommand::New { name, from, .. } => new_crew(&name, from.as_deref()),
        CrewCommand::Copy { from, to } => new_crew(&to, Some(&from)),
        CrewCommand::Rename { from, to } => rename(&from, &to),
        CrewCommand::Remove { name } => remove(&name),
        CrewCommand::Usage { crew } => usage(crew.as_deref()),
        CrewCommand::Recommend { crew } => recommend(crew.as_deref()),
    }
}

// ---------------------------------------------------------------------------
// The session's crew and foreman
// ---------------------------------------------------------------------------

/// This process's crew, once a session has chosen or switched one. One worker
/// process is one session, so a process-wide value is the session's value.
static SESSION_CREW: RwLock<Option<String>> = RwLock::new(None);
/// The model this session's foreman is running on right now (set at startup and
/// on every `/model` or crew switch), so `<default>` slots follow it.
static SESSION_FOREMAN: RwLock<Option<String>> = RwLock::new(None);

/// The crew this session routes with: a switch made in this session, else
/// `COWBOY_CREW`, else `None` (the file's active crew).
pub fn session_crew_name() -> Option<String> {
    if let Some(n) = SESSION_CREW
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return Some(n);
    }
    std::env::var(ENV_CREW).ok().filter(|s| !s.is_empty())
}

/// Switch this session's crew.
pub fn set_session_crew(name: &str) {
    *SESSION_CREW
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name.to_string());
}

/// Record the session's current foreman model.
pub fn set_session_foreman(name: &str) {
    *SESSION_FOREMAN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name.to_string());
}

/// The effective roster for this session (with its name), or `None` with no crews.
pub fn session_crew() -> Result<Option<CrewConfig>> {
    Ok(crew::load_named(session_crew_name().as_deref())?)
}

/// The crew names in crew.yaml (empty without one).
pub fn crew_names() -> Vec<String> {
    crew::load_file()
        .ok()
        .flatten()
        .map(|f| f.crews.into_keys().collect())
        .unwrap_or_default()
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
/// `COWBOY_MODEL` (set by subagent routing, always `provider/name`) wins. Otherwise
/// the session's crew's `foreman:`, when crew mode is on and the crew names one.
/// Otherwise `None` → the models.yaml default. A value that is not
/// `provider/name` is treated like any other stale override: resolution falls back
/// to the default.
pub fn session_model_override() -> Option<ModelRef> {
    match std::env::var("COWBOY_MODEL") {
        Ok(m) if !m.is_empty() => ModelRef::parse(&m).ok(),
        _ => session_crew()
            .ok()
            .flatten()
            .filter(CrewConfig::enabled)
            .and_then(|c| c.foreman),
    }
}

/// The foreman model name for delegation routing (fills `<default>` slots): what
/// this session's foreman is running on now, else a routed `COWBOY_MODEL`, else
/// the crew's foreman, else the configured default model.
pub fn foreman_model() -> Option<String> {
    if let Some(f) = SESSION_FOREMAN
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return Some(f);
    }
    session_model_override()
        .map(|r| r.to_string())
        .or_else(default_model_name)
}

/// True when crew delegation is on (crews exist and `delegation.enabled`).
pub fn crew_enabled() -> bool {
    matches!(crew::load_file(), Ok(Some(f)) if f.delegation.enabled && !f.crews.is_empty())
}

/// The active crew (what new sessions start with), if any.
pub fn active_crew() -> Option<String> {
    crew::load_file()
        .ok()
        .flatten()
        .and_then(|f| f.active_name().map(str::to_string))
}

fn load_file_or_explain() -> Result<CrewFile> {
    match crew::load_file()? {
        Some(f) if !f.crews.is_empty() => Ok(f),
        _ => bail!("no crews yet — create one with `cowboy crew init`"),
    }
}

/// Turn crew mode on/off (the `/model` picker's Solo/crew choice). Enabling with
/// no crews yet bootstraps a `default` crew from the model price tiers; disabling
/// just flips the flag and keeps the crews.
pub fn set_crew_enabled(enabled: bool) -> Result<()> {
    match crew::load_file()? {
        Some(mut f) => {
            f.delegation.enabled = enabled;
            crew::save_file(&f, |t| crew_text::set_enabled(t, enabled))?;
        }
        None if enabled => {
            let mut f = CrewFile {
                active: Some("default".into()),
                ..CrewFile::default()
            };
            f.crews.insert("default".into(), tiered_crew()?);
            crew::save_file(&f, |_| None)?;
        }
        None => {} // already solo
    }
    Ok(())
}

/// Make `name` the active crew and turn crew mode on.
pub fn set_active_crew(name: &str) -> Result<()> {
    let mut f = load_file_or_explain()?;
    if !f.crews.contains_key(name) {
        bail!("no crew `{name}`; crews: {}", names_of(&f));
    }
    f.active = Some(name.to_string());
    f.delegation.enabled = true;
    crew::save_file(&f, |t| {
        crew_text::set_active(t, Some(name)).and_then(|t| crew_text::set_enabled(&t, true))
    })?;
    Ok(())
}

fn names_of(f: &CrewFile) -> String {
    f.crews.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// The configured default model name (project default, else user default) —
/// this is the foreman when nothing else names one.
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

/// A crew whose tiers (cheapest / middle / priciest) come from the model prices.
fn tiered_crew() -> Result<CrewDef> {
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
    println!("  tiers: cheap={cheap}  standard={standard}  premium={premium}");
    Ok(CrewDef::from_roster(&crew::default_with_tiers(
        &cheap, &standard, &premium,
    )))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn init(force: bool) -> Result<()> {
    let path = crew::path().context("cannot resolve home config dir")?;
    if path.exists() {
        if !force {
            bail!(
                "crew.yaml already exists at {} — add a crew with `cowboy crew new <name>`, \
                 or use --force to start over",
                path.display()
            );
        }
        // The crews are hand-tuned in practice — which model each role gets, and the
        // delegation grants — so regenerating them from price tiers discards choices
        // the template cannot reconstruct.
        crate::ui::warn(&format!(
            "--force will replace every crew in {}",
            path.display()
        ));
        if !crate::prompt::confirm_destructive("Overwrite crew.yaml?")? {
            return Ok(());
        }
    }
    let mut f = CrewFile {
        active: Some("default".into()),
        ..CrewFile::default()
    };
    f.crews.insert("default".into(), tiered_crew()?);
    crew::save_file(&f, |_| None)?;
    crate::ui::ok(&format!("wrote crew `default` to {}", path.display()));
    println!(
        "  foreman: your selected model ({}) — change it with `cowboy models use` or /model,",
        foreman_model().as_deref().unwrap_or("none set")
    );
    println!("  or give the crew its own with `foreman: <provider/name>` in crew.yaml.");
    println!("  add more crews with `cowboy crew new <name>`; review with `cowboy crew list`.");
    Ok(())
}

fn list(name: Option<&str>) -> Result<()> {
    let f = load_file_or_explain()?;
    let active = f.active_name().map(str::to_string);
    println!(
        "crews ({}):",
        if f.delegation.enabled {
            "crew mode on"
        } else {
            "crew mode off — solo"
        }
    );
    for (n, def) in &f.crews {
        let mark = if Some(n) == active.as_ref() { "*" } else { " " };
        let foreman = def
            .foreman
            .as_ref()
            .map(|r| format!("  foreman: {r}"))
            .unwrap_or_default();
        println!("  {mark} {n}{foreman}");
    }
    println!("  (* = active: what new sessions start with)\n");

    let shown = name.map(str::to_string).or(active);
    let Some(shown) = shown else {
        println!("no active crew; pick one with `cowboy crew use <name>`");
        return Ok(());
    };
    let cfg = f
        .resolve(&shown)
        .with_context(|| format!("no crew `{shown}`; crews: {}", names_of(&f)))?;
    let foreman = cfg
        .foreman
        .as_ref()
        .map(ToString::to_string)
        .or_else(default_model_name)
        .unwrap_or_else(|| "<default>".to_string());
    println!("crew {shown} — foreman: {foreman}");
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
    let path = crew::path().context("cannot resolve home config dir")?;
    let _ = load_file_or_explain()?;
    print!("{}", std::fs::read_to_string(&path)?);
    Ok(())
}

/// Every reason the crews cannot run, one line each.
pub fn crew_problems(f: &CrewFile) -> Result<Vec<String>> {
    let models = merged_models()?;
    let mut names = model_names(&models);
    // A slot may name a harness (harnesses.yaml) as well as a model — but never a
    // name that is both, which would make the route ambiguous.
    let harnesses =
        cowboy_core::harness::HarnessesConfig::load_user().context("loading harnesses.yaml")?;
    let clash = cowboy_core::harness::colliding_names(names.iter().map(String::as_str), &harnesses);
    if !clash.is_empty() {
        bail!(
            "{} defined in both models.yaml and harnesses.yaml — rename one so a crew slot is \
             unambiguous",
            clash.join(", ")
        );
    }
    names.extend(harnesses.names().map(str::to_string));
    // Each provider a crew routes to (or runs its foreman on) must exist (and, for a
    // ChatGPT sign-in, be signed in) — otherwise every subagent sent there fails at
    // its first turn.
    let providers =
        cowboy_core::config::ProvidersConfig::load_global().context("loading providers.yaml")?;
    let mut out = Vec::new();
    for cfg in f.all() {
        if let Err(e) = cfg.validate(&names) {
            out.push(format!("crew `{}`: {e}", cfg.name));
        }
        if let Some(fm) = &cfg.foreman {
            if !models.contains_key(fm) {
                out.push(format!(
                    "crew `{}`: foreman `{fm}` is not in models.yaml",
                    cfg.name
                ));
            }
        }
        for p in provider_problems(&cfg, &providers) {
            out.push(format!("crew `{}`: {p}", cfg.name));
        }
    }
    Ok(out)
}

fn validate() -> Result<()> {
    let f = load_file_or_explain()?;
    let problems = crew_problems(&f).map_err(|e| anyhow::anyhow!("crew roster invalid: {e}"))?;
    if !problems.is_empty() {
        bail!("crew roster invalid: {}", problems.join("; "));
    }
    crate::ui::ok(&format!(
        "{} crew(s) valid ({})",
        f.crews.len(),
        names_of(&f)
    ));
    Ok(())
}

fn use_crew(name: &str) -> Result<()> {
    set_active_crew(name)?;
    crate::ui::ok(&format!(
        "new sessions start with crew `{name}` (crew mode on)"
    ));
    println!("  running sessions keep theirs; switch one with /crew {name}");
    Ok(())
}

fn solo() -> Result<()> {
    set_crew_enabled(false)?;
    crate::ui::ok("crew mode off: the foreman works solo (`cowboy crew use <name>` to turn it on)");
    Ok(())
}

fn check_new_name(f: &CrewFile, name: &str) -> Result<()> {
    if !crew::valid_crew_name(name) {
        bail!("crew name {name:?} must be non-empty, with no `/` or spaces");
    }
    if f.crews.contains_key(name) {
        bail!("crew `{name}` already exists");
    }
    Ok(())
}

fn new_crew(name: &str, from: Option<&str>) -> Result<()> {
    let mut f = crew::load_file()?.unwrap_or_default();
    check_new_name(&f, name)?;
    let first = f.crews.is_empty();
    match from {
        Some(src) => {
            let def = f
                .crews
                .get(src)
                .cloned()
                .with_context(|| format!("no crew `{src}`; crews: {}", names_of(&f)))?;
            f.crews.insert(name.to_string(), def);
            if first {
                f.active = Some(name.to_string());
            }
            crew::save_file(&f, |t| crew_text::copy_crew(t, src, name))?;
            crate::ui::ok(&format!("crew `{name}` is a copy of `{src}`"));
        }
        None => {
            let def = tiered_crew()?;
            let body = serde_yaml_ng::to_string(&def)?;
            f.crews.insert(name.to_string(), def);
            if first {
                f.active = Some(name.to_string());
            }
            crew::save_file(&f, |t| crew_text::append_crew(t, name, &body))?;
            crate::ui::ok(&format!(
                "added crew `{name}` from your models' price tiers"
            ));
        }
    }
    println!("  edit it in crew.yaml; try it in a session with /crew {name}");
    Ok(())
}

/// The only provider that signs in with ChatGPT, or `name` if it is one.
fn chatgpt_provider(name: Option<&str>) -> Result<String> {
    use cowboy_core::config::ProviderAuth;
    let providers = cowboy_core::config::ProvidersConfig::load_global()?;
    let chat: Vec<&String> = providers
        .providers
        .iter()
        .filter(|(_, p)| p.auth == ProviderAuth::Chatgpt)
        .map(|(n, _)| n)
        .collect();
    match name {
        Some(n) if chat.iter().any(|c| *c == n) => Ok(n.to_string()),
        Some(n) => bail!("provider `{n}` does not sign in with ChatGPT"),
        None => match chat.as_slice() {
            [one] => Ok((*one).clone()),
            [] => {
                bail!("no ChatGPT provider; add one with `cowboy providers add chatgpt --chatgpt`")
            }
            many => bail!(
                "several ChatGPT providers ({}); pick one with --provider",
                many.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// `crew new <name> --preset chatgpt`: the built-in all-OpenAI crew, filled with
/// the newest model of each family the account lists. Models it needs that are
/// not configured yet are imported first.
pub async fn add_chatgpt_crew(name: &str, provider: Option<&str>) -> Result<()> {
    use cowboy_core::config::{commit_model_setup_pair, load_model_setup_state, ProvidersConfig};
    use cowboy_core::crew_presets::{chatgpt_crew, chatgpt_models};
    let provider = chatgpt_provider(provider)?;
    let mut f = crew::load_file()?.unwrap_or_default();
    check_new_name(&f, name)?;
    let providers = ProvidersConfig::load_global()?;
    let p = &providers.providers[&provider];
    let entries = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        cowboy_core::model::list_models(&provider, p),
    )
    .await
    .map_err(|_| anyhow::anyhow!("`{provider}`'s catalogue did not answer in 20s"))?
    .with_context(|| format!("reading `{provider}`'s model catalogue"))?;
    let (ids, notes) = chatgpt_models(&entries).with_context(|| {
        format!("`{provider}` lists no luna, terra, sol or astra model, so the preset has nothing to use")
    })?;

    // Make sure each model the crew names is configured.
    let providers_path = ProvidersConfig::global_path().context("no home config dir")?;
    let models_path = ModelsConfig::user_path().context("no home config dir")?;
    let state = load_model_setup_state(&providers_path, &models_path)?;
    let wanted: Vec<_> = entries
        .iter()
        .filter(|e| ids.values().any(|id| *id == e.id))
        .cloned()
        .collect();
    let plan = cowboy_core::model_import::plan_import(&provider, &wanted, &state.models);
    let mut models = state.models.clone();
    if !plan.is_empty() {
        for (r, d) in &plan {
            println!("  importing {r} ({})", d.model);
            models.insert(r.clone(), d.clone());
        }
        commit_model_setup_pair(
            &providers_path,
            &models_path,
            &state.generation,
            &state.providers,
            &models,
        )?;
    }
    let mut refs = BTreeMap::new();
    for (family, id) in &ids {
        let r = models
            .iter()
            .find(|(r, d)| r.provider == provider && d.model == *id)
            .map(|(r, _)| r)
            .with_context(|| format!("{id} is not configured on `{provider}`"))?;
        refs.insert(*family, r);
    }
    let def = chatgpt_crew(&refs);
    let body = serde_yaml_ng::to_string(&def)?;
    if f.crews.is_empty() {
        f.active = Some(name.to_string());
    }
    f.crews.insert(name.to_string(), def);
    crew::save_file(&f, |t| crew_text::append_crew(t, name, &body))?;
    for n in &notes {
        println!("  note: {n}");
    }
    crate::ui::ok(&format!(
        "added crew `{name}`: luna {}, terra {}, sol {} (foreman), astra {}",
        refs["luna"], refs["terra"], refs["sol"], refs["astra"]
    ));
    if let Ok(problems) = crew_problems(&f) {
        for p in problems
            .iter()
            .filter(|p| p.contains(&format!("crew `{name}`")))
        {
            crate::ui::warn(p);
        }
    }
    println!("  try it with /crew {name}; make it the default with `cowboy crew use {name}`");
    Ok(())
}

/// Offer the ChatGPT crew after a ChatGPT sign-in (interactive only; never an
/// error — the sign-in already succeeded).
pub async fn offer_chatgpt_crew(provider: &str) {
    use std::io::IsTerminal;
    let name = "openai";
    if crew_names().iter().any(|n| n == name) {
        return; // already set up; nothing to offer
    }
    if !std::io::stdin().is_terminal() {
        println!("  add an all-OpenAI crew with `cowboy crew new {name} --preset chatgpt`");
        return;
    }
    match crate::prompt::confirm(
        &format!("Add an `{name}` crew made of your ChatGPT models?"),
        true,
    ) {
        Ok(true) => {
            if let Err(e) = add_chatgpt_crew(name, Some(provider)).await {
                crate::ui::warn(&format!(
                    "could not add the crew ({e:#}); retry with `cowboy crew new {name} --preset chatgpt`"
                ));
            }
        }
        _ => println!("  later: `cowboy crew new {name} --preset chatgpt`"),
    }
}

fn rename(from: &str, to: &str) -> Result<()> {
    let mut f = load_file_or_explain()?;
    check_new_name(&f, to)?;
    let def = f
        .crews
        .remove(from)
        .with_context(|| format!("no crew `{from}`; crews: {}", names_of(&f)))?;
    f.crews.insert(to.to_string(), def);
    if f.active.as_deref() == Some(from) {
        f.active = Some(to.to_string());
    }
    crew::save_file(&f, |t| crew_text::rename_crew(t, from, to))?;
    crate::ui::ok(&format!("renamed crew `{from}` to `{to}`"));
    println!("  sessions running `{from}` keep routing with its old definition until switched");
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let mut f = load_file_or_explain()?;
    if !f.crews.contains_key(name) {
        bail!("no crew `{name}`; crews: {}", names_of(&f));
    }
    if f.active_name() == Some(name) {
        bail!("`{name}` is the active crew; `cowboy crew use <another>` first");
    }
    if !crate::prompt::confirm_destructive(&format!("Remove crew `{name}`?"))? {
        return Ok(());
    }
    f.crews.remove(name);
    crew::save_file(&f, |t| crew_text::remove_crew(t, name))?;
    crate::ui::ok(&format!("removed crew `{name}`"));
    Ok(())
}

/// Every provider-level reason a roster's model slots (and foreman) cannot run,
/// one line each: a provider missing from `providers.yaml`, or a ChatGPT
/// provider nobody has signed in to. Harness and `<default>` slots carry no
/// provider and are skipped.
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
    if let Some(fm) = &cfg.foreman {
        used.entry(fm.provider.clone())
            .or_default()
            .insert("foreman".into());
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
fn sign_in_problem(name: &str, provider: &cowboy_core::config::Provider) -> Option<String> {
    use crate::cmd::providers_login::{sign_in_status, SignIn};
    if provider.auth != cowboy_core::config::ProviderAuth::Chatgpt {
        return None;
    }
    match sign_in_status(name) {
        SignIn::Ready(_) => None,
        SignIn::NoPlanUsage(_) => Some(format!(
            "is signed in without ChatGPT plan usage (`cowboy providers login {name}`)"
        )),
        SignIn::None => Some(format!(
            "is not signed in (`cowboy providers login {name}`)"
        )),
        SignIn::Unreadable(e) => Some(format!("has an unreadable sign-in record: {e}")),
    }
}

/// History, optionally only what ran under `crew`.
fn history(crew_filter: Option<&str>) -> Vec<crew::CrewOutcome> {
    let all = crew::load_history();
    match crew_filter {
        Some(c) => all.into_iter().filter(|o| o.crew == c).collect(),
        None => all,
    }
}

fn usage(crew_filter: Option<&str>) -> Result<()> {
    let rows = crew::usage_by_model(&history(crew_filter));
    if rows.is_empty() {
        println!(
            "no recorded crew activity{} yet (delegations are logged as they run)",
            crew_filter
                .map(|c| format!(" for crew `{c}`"))
                .unwrap_or_default()
        );
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

fn recommend(crew_filter: Option<&str>) -> Result<()> {
    let outcomes = history(crew_filter);
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

    #[test]
    fn a_crews_foreman_counts_as_a_provider_it_uses() {
        let mut cfg: CrewConfig =
            serde_yaml_ng::from_str("crew:\n  general: bifrost/glm\n").unwrap();
        cfg.foreman = Some(ModelRef::parse("openai/gpt").unwrap());
        let mut providers = ProvidersConfig::default();
        providers
            .providers
            .insert("bifrost".into(), Provider::api_key("http://x/v1", ""));
        let p = provider_problems(&cfg, &providers);
        assert!(
            p.len() == 1 && p[0].contains("`openai`") && p[0].contains("foreman"),
            "{p:?}"
        );
    }
}
