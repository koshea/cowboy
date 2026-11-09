//! `cowboy models` — configure model providers and models.
//!
//! **Providers** (endpoint + API key) are host-owned and live only in the home
//! dir (`~/.config/cowboy/providers.yaml`, mode `0600`); the agent can never
//! reach them. **Models** are grouped under their provider and named
//! `provider/name`; they may be defined at the user level
//! (`~/.config/cowboy/models.yaml`) or per project (`.cowboy/models.yaml`), project
//! entries overriding user entries by `provider/name`.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cowboy_core::config::{
    commit_model_setup_pair, default_model_ref, expand_env, load_model_setup_state, merged_models,
    resolve_model, valid_ref_part, ConfigPaths, ModelDef, ModelRef, ModelsConfig, Provider,
    ProvidersConfig, ReasoningEffort,
};
use cowboy_core::model::list_models;
use cowboy_core::model_defaults;

use crate::cli::{ModelsArgs, ModelsCommand};
use crate::style;

pub async fn run(args: ModelsArgs) -> Result<()> {
    match args.command {
        None | Some(ModelsCommand::List) => list(),
        Some(ModelsCommand::Setup) => setup().await,
        Some(ModelsCommand::Migrate { yes }) => crate::cmd::models_migrate::run(yes),
        Some(ModelsCommand::Use { name, global }) => use_default(&name, global),
        Some(ModelsCommand::Available { all }) => available(all).await,
        Some(ModelsCommand::Add {
            id,
            name,
            temp,
            context,
            max_output,
            reasoning,
            default,
        }) => add(AddArgs {
            id,
            name,
            temp,
            context,
            max_output,
            reasoning,
            default,
        }),
    }
}

// --- interactive setup ---

const CATALOGUE_TIMEOUT: Duration = Duration::from_secs(8);
const CATALOGUE_LIMIT: usize = 50;

struct SetupPlan {
    provider_name: String,
    provider: Provider,
    model_ref: ModelRef,
    model: ModelDef,
}

async fn setup() -> Result<()> {
    let providers_path =
        ProvidersConfig::global_path().context("cannot resolve home config directory")?;
    let models_path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let state = load_model_setup_state(&providers_path, &models_path)?;

    println!("Configure a model (saved to your home dir, never a project).\n");
    // Reuse a configured provider, or add a new one. Never replaces a provider:
    // with several endpoints configured, re-running setup to add a model must not
    // clobber the endpoint (and key) another set of models depends on.
    let existing: Vec<&String> = state.providers.providers.keys().collect();
    let provider_name = if existing.is_empty() {
        prompt_required("Provider name", Some("default"), valid_part)?
    } else {
        println!(
            "Configured providers: {}",
            existing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        prompt_required(
            "Provider (one of those, or a new name to add one)",
            Some(existing[0].as_str()),
            valid_part,
        )?
    };
    let provider = match state.providers.providers.get(&provider_name) {
        Some(p) => {
            println!("Using provider `{provider_name}` ({}).", p.base_url);
            p.clone()
        }
        None => prompt_new_provider(&provider_name)?,
    };
    println!("\nChecking the endpoint model catalogue (up to 8 seconds)…");
    let model_id = choose_model_id(&provider_name, &provider).await?;
    let defaults = model_defaults::lookup(&model_id);
    let model_name = prompt_required(
        &format!("Model name (saved as `{provider_name}/<name>`)"),
        Some(&sanitize_part(&defaults.name)),
        valid_part,
    )?;
    let model_ref = ModelRef::new(provider_name.clone(), model_name)?;

    let mut temperature = defaults.temperature;
    let mut max_tokens = defaults.max_tokens;
    let mut context_window = defaults.context_window;
    let mut reasoning_effort = defaults.reasoning_effort;
    if crate::prompt::confirm("Configure advanced model tuning?", false)? {
        temperature = prompt_value("Temperature", temperature, parse_temperature)?;
        max_tokens = prompt_value("Max output tokens", max_tokens, parse_positive_u32)?;
        context_window = prompt_value("Context window", context_window, parse_positive_u32)?;
        let reasoning_default = reasoning_effort
            .map(ReasoningEffort::as_str)
            .unwrap_or("none");
        reasoning_effort = loop {
            let raw = prompt("Reasoning effort", Some(reasoning_default))?;
            match parse_reasoning(&raw) {
                Ok(value) => break value,
                Err(error) => crate::ui::warn(&format!("{error}; try again")),
            }
        };
    }

    let plan = SetupPlan {
        provider_name: provider_name.clone(),
        provider,
        model_ref: model_ref.clone(),
        model: ModelDef {
            model: model_id,
            temperature,
            max_tokens,
            context_window,
            reasoning_effort,
            top_p: None,
            stop: Vec::new(),
            extra: BTreeMap::new(),
            input_cost_per_mtok: defaults.input_cost_per_mtok,
            output_cost_per_mtok: defaults.output_cost_per_mtok,
            cached_input_cost_per_mtok: defaults.cached_input_cost_per_mtok,
            headers: BTreeMap::new(),
            anthropic_cache: false,
            stream_idle_timeout_seconds: None,
        },
    };

    let replacements = replacements(&state.providers, &state.models, &plan);
    if !replacements.is_empty()
        && !crate::prompt::confirm_destructive(&format!(
            "Replace existing {}?",
            replacements.join(" and ")
        ))?
    {
        return Ok(());
    }

    let (providers, models) = apply_setup(&state.providers, &state.models, plan);
    commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &providers,
        &models,
    )?;

    crate::ui::ok(&format!(
        "saved provider `{provider_name}` and model `{model_ref}`"
    ));
    println!("  credentials: {} (mode 0600)", providers_path.display());
    println!("  models: {}", models_path.display());
    println!("\n{}", style::success("Done — you can run `cowboy` now."));
    println!("  review it with `cowboy models`, or verify the host with `cowboy doctor`");
    Ok(())
}

fn replacements(
    providers: &ProvidersConfig,
    models: &ModelsConfig,
    plan: &SetupPlan,
) -> Vec<String> {
    let mut found = Vec::new();
    if providers.providers.contains_key(&plan.provider_name) {
        found.push(format!("provider `{}`", plan.provider_name));
    }
    if models.get(&plan.model_ref).is_some() {
        found.push(format!("model `{}`", plan.model_ref));
    }
    found
}

fn apply_setup(
    existing_providers: &ProvidersConfig,
    existing_models: &ModelsConfig,
    plan: SetupPlan,
) -> (ProvidersConfig, ModelsConfig) {
    let mut providers = existing_providers.clone();
    let mut models = existing_models.clone();
    let first = models.is_empty();
    providers
        .providers
        .insert(plan.provider_name, plan.provider);
    models.insert(plan.model_ref.clone(), plan.model);
    if first || models.default.is_none() {
        models.default = Some(plan.model_ref);
    }
    (providers, models)
}

async fn choose_model_id(name: &str, provider: &Provider) -> Result<String> {
    let entries = tokio::time::timeout(CATALOGUE_TIMEOUT, list_models(name, provider)).await;
    let mut entries = match entries {
        Ok(Ok(entries)) => entries
            .into_iter()
            .filter(|entry| model_defaults::is_chat(&entry.id))
            .collect::<Vec<_>>(),
        Ok(Err(_)) => {
            crate::ui::warn(
                "the endpoint did not provide a usable catalogue; enter a model id manually",
            );
            return prompt_required("Model id", None, valid_name);
        }
        Err(_) => {
            crate::ui::warn("catalogue lookup timed out; enter a model id manually");
            return prompt_required("Model id", None, valid_name);
        }
    };
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    entries.dedup_by(|left, right| left.id == right.id);
    entries.truncate(CATALOGUE_LIMIT);
    if entries.is_empty() {
        crate::ui::warn("the endpoint returned no chat models; enter a model id manually");
        return prompt_required("Model id", None, valid_name);
    }

    println!("Choose a model, or enter its provider id manually:");
    for (index, entry) in entries.iter().enumerate() {
        println!("  {:>2}. {}", index + 1, entry.id);
    }
    loop {
        let raw = prompt("Model number or id", None)?;
        if let Ok(index) = raw.parse::<usize>() {
            if let Some(entry) = index.checked_sub(1).and_then(|i| entries.get(i)) {
                return Ok(entry.id.clone());
            }
            crate::ui::warn("that catalogue number is not available; try again");
        } else if valid_name(&raw).is_ok() {
            return Ok(raw);
        } else {
            crate::ui::warn("a model id is required; try again");
        }
    }
}

fn valid_name(value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() {
        Err("a value is required".into())
    } else {
        Ok(())
    }
}

/// A provider or model name: one half of `provider/name`.
fn valid_part(value: &str) -> std::result::Result<(), String> {
    if valid_ref_part(value) {
        Ok(())
    } else {
        Err("use a name with no `/` and no surrounding spaces".into())
    }
}

/// A suggested name made usable as one half of `provider/name`.
fn sanitize_part(name: &str) -> String {
    name.trim().replace('/', "-")
}

pub(crate) fn validate_base_url(value: &str) -> std::result::Result<(), String> {
    let expanded = expand_env(value).map_err(|error| error.to_string())?;
    let url =
        reqwest::Url::parse(&expanded).map_err(|_| "enter a valid absolute URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("the endpoint URL must use http or https".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("do not put credentials in the endpoint URL".into());
    }
    if url.host_str().is_none() {
        return Err("the endpoint URL must include a host".into());
    }
    Ok(())
}

fn parse_temperature(value: &str) -> std::result::Result<f32, String> {
    let parsed = value
        .parse::<f32>()
        .map_err(|_| "temperature must be a number".to_string())?;
    if parsed.is_finite() && (0.0..=2.0).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("temperature must be between 0 and 2".into())
    }
}

fn parse_positive_u32(value: &str) -> std::result::Result<u32, String> {
    match value.parse::<u32>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err("enter a positive whole number".into()),
    }
}

fn prompt_required(
    label: &str,
    default: Option<&str>,
    validate: impl Fn(&str) -> std::result::Result<(), String>,
) -> Result<String> {
    loop {
        let value = prompt(label, default)?;
        match validate(&value) {
            Ok(()) => return Ok(value),
            Err(error) => crate::ui::warn(&format!("{error}; try again")),
        }
    }
}

fn prompt_value<T: std::fmt::Display>(
    label: &str,
    default: T,
    parse: impl Fn(&str) -> std::result::Result<T, String>,
) -> Result<T> {
    loop {
        let raw = prompt(label, Some(&default.to_string()))?;
        match parse(&raw) {
            Ok(value) => return Ok(value),
            Err(error) => crate::ui::warn(&format!("{error}; try again")),
        }
    }
}

/// Ask for a new provider's endpoint, wire API and key.
fn prompt_new_provider(name: &str) -> Result<Provider> {
    let base_url = prompt_required(
        "Endpoint base URL (e.g. https://host/v1)",
        None,
        validate_base_url,
    )?;
    let api = loop {
        let raw = prompt(
            "API it speaks (chat_completions or responses)",
            Some("chat_completions"),
        )?;
        match raw.trim() {
            "chat_completions" | "chat" => break cowboy_core::config::WireApi::ChatCompletions,
            "responses" => break cowboy_core::config::WireApi::Responses,
            _ => crate::ui::warn("enter chat_completions or responses"),
        }
    };
    let api_key = read_secret(&format!(
        "API key for {name} (leave empty if the endpoint needs none)"
    ))?;
    Ok(Provider {
        api,
        ..Provider::api_key(base_url, api_key.trim())
    })
}

// --- list ---

fn list() -> Result<()> {
    let providers = ProvidersConfig::load_global()?;
    let user = ModelsConfig::user_path()
        .map(|p| ModelsConfig::load_opt(&p))
        .transpose()?
        .flatten();
    let project = project_models()?;

    println!("{}", style::bold("providers (home-only):"));
    if providers.providers.is_empty() {
        println!("  {}", style::dim("(none — run `cowboy models setup`)"));
    } else {
        for (name, p) in &providers.providers {
            // Never print the key, only how the provider authenticates.
            println!(
                "  {name:<14} {}  api: {}  auth: {}",
                p.base_url,
                p.api.as_str(),
                crate::cmd::providers::auth_summary(name, p)
            );
        }
    }

    println!("\n{}", style::bold("models:"));
    let merged = merged_models(user.as_ref(), project.as_ref());
    if merged.is_empty() {
        println!("  {}", style::dim("(none — run `cowboy models setup`)"));
    } else {
        let mut current: Option<&str> = None;
        for (r, def) in &merged {
            if current != Some(r.provider.as_str()) {
                current = Some(r.provider.as_str());
                let missing = if providers.providers.contains_key(&r.provider) {
                    ""
                } else {
                    "  (provider not configured)"
                };
                println!("  {}{missing}", style::bold(&format!("{}/", r.provider)));
            }
            // Project overrides user; report the effective source.
            let src = if project.as_ref().is_some_and(|p| p.get(r).is_some()) {
                "project"
            } else {
                "user"
            };
            println!("    {:<30} {}  [{src}]", r.to_string(), def.model);
        }
    }

    let default = default_model_ref(user.as_ref(), project.as_ref());
    println!(
        "\ndefault: {}",
        default
            .map(|d| d.to_string())
            .unwrap_or_else(|| "(none set — `cowboy models use <provider/name>`)".into())
    );

    // Confirm the default actually resolves to a provider.
    match resolve_model(&providers, user.as_ref(), project.as_ref(), None) {
        Ok(m) => println!("resolves to: {} @ {}", m.model, m.base_url),
        Err(e) => println!("{}", style::warning(&format!("note: {e}"))),
    }
    Ok(())
}

// --- use ---

/// Persist the user-level default model (no stdout output) — used by the TUI
/// `/model` picker so the selection survives restarts and the crew foreman
/// reflects it. Assumes `name` is a known model.
pub fn set_user_default(name: &str) -> Result<()> {
    let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let mut cfg = ModelsConfig::load_opt(&path)?.unwrap_or_default();
    cfg.default = Some(ModelRef::parse(name)?);
    cfg.save(&path)?;
    Ok(())
}

fn use_default(name: &str, global: bool) -> Result<()> {
    let user = ModelsConfig::user_path()
        .map(|p| ModelsConfig::load_opt(&p))
        .transpose()?
        .flatten();
    let project = project_models()?;

    // The name must exist in the merged set.
    let r = ModelRef::parse(name)?;
    if !merged_models(user.as_ref(), project.as_ref()).contains_key(&r) {
        bail!("unknown model `{r}`; see `cowboy models list`");
    }
    let name = r.to_string();

    if global {
        let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
        let mut cfg = user.unwrap_or_default();
        cfg.default = Some(r.clone());
        cfg.save(&path)?;
        crate::ui::ok(&format!(
            "user default is now `{name}` ({})",
            path.display()
        ));
    } else {
        let paths = ConfigPaths::for_root(crate::cmd::project_root()?);
        let mut cfg = project.unwrap_or_default();
        cfg.default = Some(r);
        cfg.save(&paths.models)?;
        crate::ui::ok(&format!(
            "project default is now `{name}` ({})",
            paths.models.display()
        ));
    }
    Ok(())
}

// --- available (list the provider catalogue) ---

async fn available(all: bool) -> Result<()> {
    let providers = ProvidersConfig::load_global()?;
    if providers.providers.is_empty() {
        bail!("no providers configured; run `cowboy models setup`");
    }
    // Provider-side ids already registered (for the [configured] marker).
    let user = ModelsConfig::user_path()
        .map(|p| ModelsConfig::load_opt(&p))
        .transpose()?
        .flatten();
    let project = project_models()?;
    // (provider, provider-side id) — the same id on two providers is two models.
    let configured: std::collections::BTreeMap<(String, String), ModelRef> =
        merged_models(user.as_ref(), project.as_ref())
            .into_iter()
            .map(|(r, d)| ((r.provider.clone(), d.model), r))
            .collect();

    for (pname, p) in &providers.providers {
        let base = expand_env(&p.base_url).unwrap_or_else(|_| p.base_url.clone());
        println!("{}", style::bold(&format!("provider {pname} ({base}):")));
        match list_models(pname, p).await {
            Ok(mut entries) => {
                entries.sort_by(|a, b| a.id.cmp(&b.id));
                let mut shown = 0;
                for e in &entries {
                    if !all && !model_defaults::is_chat(&e.id) {
                        continue;
                    }
                    let suggested = model_defaults::lookup(&e.id).name;
                    let mark = match configured.get(&(pname.clone(), e.id.clone())) {
                        Some(r) => format!("  [configured as {r}]"),
                        None => String::new(),
                    };
                    println!("  {:<50} {suggested}{mark}", e.id);
                    shown += 1;
                }
                if shown == 0 {
                    println!("  (no chat models; pass --all to see everything)");
                }
            }
            Err(err) => println!("  {}", style::error(&format!("error: {err}"))),
        }
    }
    println!("\nRegister one with: cowboy models add <id> --name <provider/name>");
    Ok(())
}

// --- add (register a model by id, prefilled from defaults) ---

struct AddArgs {
    id: String,
    name: Option<String>,
    temp: Option<f32>,
    context: Option<u32>,
    max_output: Option<u32>,
    reasoning: Option<crate::cli::Reasoning>,
    default: bool,
}

fn add(a: AddArgs) -> Result<()> {
    let providers = ProvidersConfig::load_global()?;
    if providers.providers.is_empty() {
        bail!("no providers configured; run `cowboy models setup`");
    }
    let d = model_defaults::lookup(&a.id);
    // `--name provider/name`; without it, the only provider and the shipped name.
    let r = match &a.name {
        Some(n) => ModelRef::parse(n)?,
        None => ModelRef::new(sole_provider(&providers)?, sanitize_part(&d.name))?,
    };
    if !providers.providers.contains_key(&r.provider) {
        bail!(
            "unknown provider `{}` in `{r}`; see `cowboy models list`",
            r.provider
        );
    }
    let name = r.to_string();
    let reasoning_effort = match a.reasoning {
        Some(r) => r.effort(),
        None => d.reasoning_effort,
    };
    let def = ModelDef {
        model: a.id.clone(),
        temperature: a.temp.unwrap_or(d.temperature),
        max_tokens: a.max_output.unwrap_or(d.max_tokens),
        context_window: a.context.unwrap_or(d.context_window),
        reasoning_effort,
        top_p: None,
        stop: Vec::new(),
        extra: BTreeMap::new(),
        headers: BTreeMap::new(),
        input_cost_per_mtok: d.input_cost_per_mtok,
        output_cost_per_mtok: d.output_cost_per_mtok,
        cached_input_cost_per_mtok: d.cached_input_cost_per_mtok,
        anthropic_cache: false,
        stream_idle_timeout_seconds: None,
    };

    let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let mut cfg = ModelsConfig::load_opt(&path)?.unwrap_or_default();
    let first = cfg.is_empty();
    cfg.insert(r.clone(), def);
    if a.default || first || cfg.default.is_none() {
        cfg.default = Some(r.clone());
    }
    cfg.save(&path)?;
    crate::ui::ok(&format!(
        "saved model `{name}` ({}) to {}",
        a.id,
        path.display()
    ));
    if cfg.default.as_ref() == Some(&r) {
        println!("  (default model)");
    }
    Ok(())
}

fn parse_reasoning(s: &str) -> Result<Option<ReasoningEffort>> {
    Ok(match s.to_lowercase().as_str() {
        "none" | "off" | "" => None,
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        other => bail!("invalid reasoning effort {other:?} (none|minimal|low|medium|high)"),
    })
}

/// Default provider when one isn't named: the only one, else `default`, else err.
fn sole_provider(providers: &ProvidersConfig) -> Result<String> {
    if providers.providers.len() == 1 {
        Ok(providers.providers.keys().next().unwrap().clone())
    } else if providers.providers.contains_key("default") {
        Ok("default".to_string())
    } else {
        bail!(
            "multiple providers configured; name the model `<provider>/<name>` with one of: {}",
            providers
                .providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Write a model to the user config as `provider/name` (used by the TUI `/models`
/// form, which knows which provider's catalogue the model came from), and make it
/// the default if it's the first model. `reasoning` is
/// `none|minimal|low|medium|high`. Returns the saved reference.
pub fn save_user_model(
    provider: &str,
    name: &str,
    id: &str,
    temperature: f32,
    context_window: u32,
    max_output: u32,
    reasoning: &str,
) -> Result<ModelRef> {
    let providers = ProvidersConfig::load_global()?;
    if !providers.providers.contains_key(provider) {
        bail!("unknown provider `{provider}`; see `cowboy providers list`");
    }
    let r = ModelRef::new(provider, name)?;
    let d = model_defaults::lookup(id);
    let def = ModelDef {
        model: id.to_string(),
        temperature,
        max_tokens: max_output,
        context_window,
        reasoning_effort: parse_reasoning(reasoning)?,
        top_p: None,
        stop: Vec::new(),
        extra: BTreeMap::new(),
        headers: BTreeMap::new(),
        input_cost_per_mtok: d.input_cost_per_mtok,
        output_cost_per_mtok: d.output_cost_per_mtok,
        cached_input_cost_per_mtok: d.cached_input_cost_per_mtok,
        anthropic_cache: false,
        stream_idle_timeout_seconds: None,
    };
    let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let mut cfg = ModelsConfig::load_opt(&path)?.unwrap_or_default();
    let first = cfg.is_empty();
    cfg.insert(r.clone(), def);
    if first || cfg.default.is_none() {
        cfg.default = Some(r.clone());
    }
    cfg.save(&path)?;
    Ok(r)
}

// --- helpers ---

/// Load the project-level models file if we're in a project that has one.
fn project_models() -> Result<Option<ModelsConfig>> {
    let paths = ConfigPaths::for_root(crate::cmd::project_root()?);
    Ok(ModelsConfig::load_opt(&paths.models)?)
}

/// Read a secret. On a real terminal, use a no-echo prompt; otherwise (piped /
/// CI) read a plain line from stdin so the command stays scriptable.
pub(crate) fn read_secret(label: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{label} (input hidden): ")).context("reading secret")
    } else {
        Ok(prompt(label, None)?)
    }
}

/// Prompt for a line. Thin alias for the shared helper, kept for readability at the
/// many call sites in this module.
fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    crate::prompt::line(label, default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(url: &str) -> Provider {
        Provider::api_key(url, "secret")
    }

    fn model(id: &str) -> ModelDef {
        ModelDef {
            model: id.into(),
            temperature: 0.2,
            max_tokens: 8_192,
            context_window: 200_000,
            reasoning_effort: None,
            top_p: None,
            stop: Vec::new(),
            extra: BTreeMap::new(),
            input_cost_per_mtok: None,
            output_cost_per_mtok: None,
            cached_input_cost_per_mtok: None,
            headers: BTreeMap::new(),
            anthropic_cache: false,
            stream_idle_timeout_seconds: None,
        }
    }

    fn plan(provider_name: &str, model_name: &str) -> SetupPlan {
        SetupPlan {
            provider_name: provider_name.into(),
            provider: provider("https://new.example/v1"),
            model_ref: ModelRef::new(provider_name, model_name).unwrap(),
            model: model("new/model"),
        }
    }

    #[test]
    fn setup_merge_preserves_unrelated_entries_and_default() {
        let mut providers = ProvidersConfig::default();
        providers
            .providers
            .insert("old".into(), provider("https://old.example/v1"));
        let old = ModelRef::parse("old/old").unwrap();
        let mut models = ModelsConfig {
            default: Some(old.clone()),
            ..ModelsConfig::default()
        };
        models.insert(old.clone(), model("old/model"));

        let (updated_providers, updated_models) =
            apply_setup(&providers, &models, plan("new", "new"));
        assert!(updated_providers.providers.contains_key("old"));
        assert!(updated_providers.providers.contains_key("new"));
        assert!(updated_models.get(&old).is_some());
        assert!(updated_models
            .get(&ModelRef::parse("new/new").unwrap())
            .is_some());
        assert_eq!(updated_models.default, Some(old));
    }

    #[test]
    fn replacement_detection_names_each_collision() {
        let mut providers = ProvidersConfig::default();
        providers
            .providers
            .insert("same".into(), provider("https://old.example/v1"));
        let mut models = ModelsConfig::default();
        models.insert(ModelRef::parse("same/same").unwrap(), model("old/model"));
        let found = replacements(&providers, &models, &plan("same", "same"));
        assert_eq!(found, ["provider `same`", "model `same/same`"]);
    }

    #[test]
    fn invalid_tuning_is_rejected_instead_of_defaulted() {
        assert!(parse_temperature("warm").is_err());
        assert!(parse_temperature("NaN").is_err());
        assert!(parse_temperature("2.1").is_err());
        assert_eq!(parse_temperature("0.7").unwrap(), 0.7);
        assert!(parse_positive_u32("0").is_err());
        assert!(parse_positive_u32("many").is_err());
        assert_eq!(parse_positive_u32("4096").unwrap(), 4096);
    }

    #[test]
    fn endpoint_validation_refuses_embedded_credentials() {
        assert!(validate_base_url("not a url").is_err());
        assert!(validate_base_url("file:///tmp/api").is_err());
        assert!(validate_base_url("https://key@example.test/v1").is_err());
        assert!(validate_base_url("https://example.test/v1").is_ok());
    }
}
