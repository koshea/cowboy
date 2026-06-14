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
    resolve_model, ConfigPaths, ModelDef, ModelsConfig, Provider, ProvidersConfig,
};

use crate::cli::{ModelsArgs, ModelsCommand};

pub fn run(args: ModelsArgs) -> Result<()> {
    match args.command {
        ModelsCommand::Setup => setup(),
        ModelsCommand::List => list(),
        ModelsCommand::Use { name, global } => use_default(&name, global),
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
    println!("✓ saved provider `{pname}` to {}", providers_path.display());

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
                headers: BTreeMap::new(),
            },
        );
        // Make the first-ever model the default.
        if first || user_models.default.is_none() {
            user_models.default = Some(mname.clone());
        }
        user_models.save(&user_models_path)?;
        println!("✓ saved model `{mname}` to {}", user_models_path.display());
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

    println!("providers (home-only):");
    if providers.providers.is_empty() {
        println!("  (none — run `cowboy models setup`)");
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

    println!("\nmodels:");
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
        println!("  (none — run `cowboy models setup`)");
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
        Err(e) => println!("note: {e}"),
    }
    Ok(())
}

// --- use ---

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
        println!("✓ user default is now `{name}` ({})", path.display());
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
