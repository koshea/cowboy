//! `cowboy models` — configure model providers and models.
//!
//! **Providers** (endpoint + API key) are host-owned and live only in the home
//! dir (`~/.config/cowboy/providers.yaml`, mode `0600`); the agent can never
//! reach them. **Models** reference a provider by name and may be defined at the
//! user level (`~/.config/cowboy/models.yaml`) or per project
//! (`.cowboy/models.yaml`); project entries override user entries by name.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};
use cowboy_core::config::{
    expand_env, resolve_model, ConfigPaths, ModelDef, ModelsConfig, Provider, ProvidersConfig,
    ReasoningEffort,
};
use cowboy_core::model::list_models;
use cowboy_core::model_defaults;

use crate::cli::{ModelsArgs, ModelsCommand};
use crate::style;

pub async fn run(args: ModelsArgs) -> Result<()> {
    match args.command {
        ModelsCommand::Setup => setup(),
        ModelsCommand::List => list(),
        ModelsCommand::Use { name, global } => use_default(&name, global),
        ModelsCommand::Available { all } => available(all).await,
        ModelsCommand::Add {
            id,
            name,
            provider,
            temp,
            context,
            max_output,
            reasoning,
            default,
        } => add(AddArgs {
            id,
            name,
            provider,
            temp,
            context,
            max_output,
            reasoning,
            default,
        }),
    }
}

// --- interactive setup ---

fn setup() -> Result<()> {
    let providers_path =
        ProvidersConfig::global_path().context("cannot resolve home config directory")?;
    let user_models_path =
        ModelsConfig::user_path().context("cannot resolve home config directory")?;

    let mut providers = ProvidersConfig::load_global().unwrap_or_default();
    let mut user_models = ModelsConfig::load_opt(&user_models_path)?.unwrap_or_default();

    println!("Configure a model provider (saved to your home dir, never a project).\n");

    // --- provider ---
    let pname = prompt("Provider name", Some("default"))?;
    let base_url = prompt("Endpoint base URL (e.g. https://host/v1)", None)?;
    if base_url.is_empty() {
        bail!("a base URL is required");
    }
    let api_key = read_secret(&format!("API key for {pname}"))?;
    if api_key.trim().is_empty() {
        bail!("an API key is required");
    }
    providers.providers.insert(
        pname.clone(),
        Provider {
            base_url,
            api_key: api_key.trim().to_string(),
            headers: BTreeMap::new(),
        },
    );
    providers.save(&providers_path)?;
    println!(
        "{}",
        style::success(&format!(
            "✓ saved provider `{pname}` to {}",
            providers_path.display()
        ))
    );

    // --- model ---
    if yes_no("\nDefine a model that uses this provider now?", true)? {
        let mname = prompt("Model name", Some(&pname))?;
        let model_id = prompt("Model id (e.g. anthropic/claude-sonnet-4-6)", None)?;
        if model_id.is_empty() {
            bail!("a model id is required");
        }
        let temperature = prompt_parsed("Temperature", 0.2_f32)?;
        let max_tokens = prompt_parsed("Max tokens", 8192_u32)?;
        let context_window = prompt_parsed("Context window", 200_000_u32)?;

        let first = user_models.models.is_empty();
        user_models.models.insert(
            mname.clone(),
            ModelDef {
                provider: pname.clone(),
                model: model_id,
                temperature,
                max_tokens,
                context_window,
                reasoning_effort: None,
                top_p: None,
                stop: Vec::new(),
                extra: BTreeMap::new(),
                input_cost_per_mtok: None,
                output_cost_per_mtok: None,
                headers: BTreeMap::new(),
                anthropic_cache: false,
            },
        );
        // Make the first-ever model the default.
        if first || user_models.default.is_none() {
            user_models.default = Some(mname.clone());
        }
        user_models.save(&user_models_path)?;
        println!(
            "{}",
            style::success(&format!(
                "✓ saved model `{mname}` to {}",
                user_models_path.display()
            ))
        );
        if user_models.default.as_deref() == Some(mname.as_str()) {
            println!("  (set as the default model)");
        }
    }

    println!("\nDone. Run `cowboy models list` to review, or `cowboy doctor` to verify.");
    Ok(())
}

// --- list ---

fn list() -> Result<()> {
    let providers = ProvidersConfig::load_global().unwrap_or_default();
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
            // Never print the key, only whether one is set.
            let key = if p.api_key.is_empty() {
                "MISSING"
            } else {
                "set"
            };
            println!("  {name:<14} {}  key: {key}", p.base_url);
        }
    }

    println!("\n{}", style::bold("models:"));
    let mut names: Vec<&String> = Vec::new();
    if let Some(u) = &user {
        names.extend(u.models.keys());
    }
    if let Some(pr) = &project {
        names.extend(pr.models.keys());
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        println!("  {}", style::dim("(none — run `cowboy models setup`)"));
    } else {
        for name in names {
            // Project overrides user; report the effective source + def.
            let (def, src) = project
                .as_ref()
                .and_then(|pr| pr.models.get(name).map(|d| (d, "project")))
                .or_else(|| {
                    user.as_ref()
                        .and_then(|u| u.models.get(name).map(|d| (d, "user")))
                })
                .expect("name came from one of the maps");
            println!("  {name:<14} {} via {}  [{src}]", def.model, def.provider);
        }
    }

    let default = project
        .as_ref()
        .and_then(|p| p.default.clone())
        .or_else(|| user.as_ref().and_then(|u| u.default.clone()));
    println!(
        "\ndefault: {}",
        default
            .as_deref()
            .unwrap_or("(none set — `cowboy models use <name>`)")
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
    cfg.default = Some(name.to_string());
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
    let known = user
        .as_ref()
        .map(|u| u.models.contains_key(name))
        .unwrap_or(false)
        || project
            .as_ref()
            .map(|p| p.models.contains_key(name))
            .unwrap_or(false);
    if !known {
        bail!("unknown model {name:?}; see `cowboy models list`");
    }

    if global {
        let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
        let mut cfg = user.unwrap_or_default();
        cfg.default = Some(name.to_string());
        cfg.save(&path)?;
        println!(
            "{}",
            style::success(&format!(
                "✓ user default is now `{name}` ({})",
                path.display()
            ))
        );
    } else {
        let paths = ConfigPaths::for_root(crate::cmd::project_root()?);
        let mut cfg = project.unwrap_or_default();
        cfg.default = Some(name.to_string());
        cfg.save(&paths.models)?;
        println!(
            "✓ project default is now `{name}` ({})",
            paths.models.display()
        );
    }
    Ok(())
}

// --- available (list the provider catalogue) ---

async fn available(all: bool) -> Result<()> {
    let providers = ProvidersConfig::load_global().unwrap_or_default();
    if providers.providers.is_empty() {
        bail!("no providers configured; run `cowboy models setup`");
    }
    // Provider-side ids already registered (for the [configured] marker).
    let user = ModelsConfig::user_path()
        .map(|p| ModelsConfig::load_opt(&p))
        .transpose()?
        .flatten();
    let project = project_models()?;
    let configured: std::collections::BTreeSet<String> = user
        .iter()
        .chain(project.iter())
        .flat_map(|c| c.models.values().map(|d| d.model.clone()))
        .collect();

    for (pname, p) in &providers.providers {
        let base = expand_env(&p.base_url).unwrap_or_else(|_| p.base_url.clone());
        println!("{}", style::bold(&format!("provider {pname} ({base}):")));
        match list_models(&base, &p.api_key, &p.headers).await {
            Ok(mut entries) => {
                entries.sort_by(|a, b| a.id.cmp(&b.id));
                let mut shown = 0;
                for e in &entries {
                    if !all && !model_defaults::is_chat(&e.id) {
                        continue;
                    }
                    let suggested = model_defaults::lookup(&e.id).name;
                    let mark = if configured.contains(&e.id) {
                        "  [configured]"
                    } else {
                        ""
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
    println!("\nRegister one with: cowboy models add <id>");
    Ok(())
}

// --- add (register a model by id, prefilled from defaults) ---

struct AddArgs {
    id: String,
    name: Option<String>,
    provider: Option<String>,
    temp: Option<f32>,
    context: Option<u32>,
    max_output: Option<u32>,
    reasoning: Option<String>,
    default: bool,
}

fn add(a: AddArgs) -> Result<()> {
    let providers = ProvidersConfig::load_global().unwrap_or_default();
    if providers.providers.is_empty() {
        bail!("no providers configured; run `cowboy models setup`");
    }
    let provider = match a.provider {
        Some(p) => p,
        None if providers.providers.len() == 1 => {
            providers.providers.keys().next().unwrap().clone()
        }
        None if providers.providers.contains_key("default") => "default".to_string(),
        None => bail!(
            "multiple providers configured; pass --provider <name> ({})",
            providers
                .providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if !providers.providers.contains_key(&provider) {
        bail!("unknown provider {provider:?}; see `cowboy models list`");
    }

    let d = model_defaults::lookup(&a.id);
    let name = a.name.unwrap_or(d.name);
    let reasoning_effort = match a.reasoning {
        Some(s) => parse_reasoning(&s)?,
        None => d.reasoning_effort,
    };
    let def = ModelDef {
        provider,
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
        anthropic_cache: false,
    };

    let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let mut cfg = ModelsConfig::load_opt(&path)?.unwrap_or_default();
    let first = cfg.models.is_empty();
    cfg.models.insert(name.clone(), def);
    if a.default || first || cfg.default.is_none() {
        cfg.default = Some(name.clone());
    }
    cfg.save(&path)?;
    println!(
        "{}",
        style::success(&format!(
            "✓ saved model `{name}` ({}) to {}",
            a.id,
            path.display()
        ))
    );
    if cfg.default.as_deref() == Some(name.as_str()) {
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
            "multiple providers configured; pass --provider <name> ({})",
            providers
                .providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Write a model to the user config (used by the TUI `/models` form). Picks the
/// sole/`default` provider, applies the given settings, and makes it the default
/// if it's the first model. `reasoning` is `none|minimal|low|medium|high`.
pub fn save_user_model(
    name: &str,
    id: &str,
    temperature: f32,
    context_window: u32,
    max_output: u32,
    reasoning: &str,
) -> Result<()> {
    let providers = ProvidersConfig::load_global().unwrap_or_default();
    if providers.providers.is_empty() {
        bail!("no providers configured; run `cowboy models setup`");
    }
    let provider = sole_provider(&providers)?;
    let d = model_defaults::lookup(id);
    let def = ModelDef {
        provider,
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
        anthropic_cache: false,
    };
    let path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let mut cfg = ModelsConfig::load_opt(&path)?.unwrap_or_default();
    let first = cfg.models.is_empty();
    cfg.models.insert(name.to_string(), def);
    if first || cfg.default.is_none() {
        cfg.default = Some(name.to_string());
    }
    cfg.save(&path)?;
    Ok(())
}

// --- helpers ---

/// Load the project-level models file if we're in a project that has one.
fn project_models() -> Result<Option<ModelsConfig>> {
    let paths = ConfigPaths::for_root(crate::cmd::project_root()?);
    Ok(ModelsConfig::load_opt(&paths.models)?)
}

/// Read a secret. On a real terminal, use a no-echo prompt; otherwise (piped /
/// CI) read a plain line from stdin so the command stays scriptable.
fn read_secret(label: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{label} (input hidden): ")).context("reading secret")
    } else {
        Ok(prompt(label, None)?)
    }
}

/// Prompt for a line, returning the trimmed input (or the default on empty).
fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) => print!("{label} [{d}]: "),
        None => print!("{label}: "),
    }
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let t = line.trim();
    Ok(if t.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        t.to_string()
    })
}

/// Prompt for a value parseable to `T`, falling back to `default` on empty;
/// re-uses the default on a parse error after warning.
fn prompt_parsed<T: std::str::FromStr + std::fmt::Display>(label: &str, default: T) -> Result<T> {
    let raw = prompt(label, Some(&default.to_string()))?;
    Ok(raw.parse().unwrap_or(default))
}

fn yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim() {
        "" => default_yes,
        s => matches!(s, "y" | "Y" | "yes"),
    })
}
