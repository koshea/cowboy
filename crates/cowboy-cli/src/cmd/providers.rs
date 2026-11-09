//! `cowboy providers` — the endpoints in the home-only `providers.yaml`.
//!
//! Several providers coexist (a local gateway beside a hosted API); a model is
//! `provider/name` under the one that serves it. Every write goes through the same
//! locked, generation-checked pair commit as `cowboy models setup`, because
//! removing a provider also prunes its models from the home `models.yaml`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use cowboy_core::config::{
    commit_model_setup_pair, load_model_setup_state, merged_models, valid_ref_part, ConfigPaths,
    ModelsConfig, Provider, ProviderAuth, ProvidersConfig, WireApi,
};

use crate::cli::{ProvidersArgs, ProvidersCommand, WireApiArg};
use crate::style;

pub async fn run(args: ProvidersArgs) -> Result<()> {
    match args.command {
        None | Some(ProvidersCommand::List) => list(),
        Some(ProvidersCommand::Add {
            name,
            base_url,
            api,
            no_key,
        }) => add(&name, &base_url, api, no_key),
        Some(ProvidersCommand::Remove { name, force }) => remove(&name, force),
    }
}

impl From<WireApiArg> for WireApi {
    fn from(a: WireApiArg) -> Self {
        match a {
            WireApiArg::ChatCompletions => WireApi::ChatCompletions,
            WireApiArg::Responses => WireApi::Responses,
        }
    }
}

fn paths() -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    Ok((
        ProvidersConfig::global_path().context("cannot resolve home config directory")?,
        ModelsConfig::user_path().context("cannot resolve home config directory")?,
    ))
}

fn project_models() -> Option<ModelsConfig> {
    let root = crate::cmd::project_root().ok()?;
    ModelsConfig::load_opt(&ConfigPaths::for_root(root).models)
        .ok()
        .flatten()
}

fn list() -> Result<()> {
    let providers = ProvidersConfig::load_global()?;
    if providers.providers.is_empty() {
        println!(
            "{}",
            style::dim(
                "no providers — add one with `cowboy providers add` or `cowboy models setup`"
            )
        );
        return Ok(());
    }
    let user = ModelsConfig::user_path()
        .map(|p| ModelsConfig::load_opt(&p))
        .transpose()?
        .flatten();
    let project = project_models();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in merged_models(user.as_ref(), project.as_ref()).keys() {
        *counts.entry(r.provider.clone()).or_default() += 1;
    }
    for (name, p) in &providers.providers {
        println!("{}", style::bold(name));
        println!("  endpoint: {}", p.base_url);
        println!("  api:      {}", p.api.as_str());
        println!("  auth:     {}", auth_summary(name, p));
        println!("  models:   {}", counts.get(name).copied().unwrap_or(0));
    }
    Ok(())
}

/// How a provider authenticates, without ever printing a secret.
pub(crate) fn auth_summary(_name: &str, p: &Provider) -> String {
    match p.auth {
        ProviderAuth::ApiKey if p.api_key.is_empty() => "none (keyless)".into(),
        ProviderAuth::ApiKey => "api key (set)".into(),
        ProviderAuth::Chatgpt => "Sign in with ChatGPT".into(),
    }
}

fn add(name: &str, base_url: &str, api: WireApiArg, no_key: bool) -> Result<()> {
    if !valid_ref_part(name) {
        bail!("provider name {name:?} must be non-empty with no `/` (models are `{name}/<model>`)");
    }
    crate::cmd::models::validate_base_url(base_url).map_err(anyhow::Error::msg)?;
    let api_key = if no_key {
        String::new()
    } else {
        crate::cmd::models::read_secret(&format!("API key for {name}"))?
            .trim()
            .to_string()
    };
    let provider = Provider {
        api: api.into(),
        ..Provider::api_key(base_url, api_key)
    };
    insert_provider(name, provider)?;
    crate::ui::ok(&format!("added provider `{name}`"));
    println!("  add a model with `cowboy models add <model-id> --name {name}/<name>`,");
    println!("  or pick one from its catalogue with /models in a session");
    Ok(())
}

/// Commit a new provider through the locked pair commit. Refuses to replace one.
pub(crate) fn insert_provider(name: &str, provider: Provider) -> Result<()> {
    let (providers_path, models_path) = paths()?;
    let state = load_model_setup_state(&providers_path, &models_path)?;
    if state.providers.providers.contains_key(name) {
        bail!("provider `{name}` already exists; remove it first with `cowboy providers remove {name}`");
    }
    let mut providers = state.providers.clone();
    providers.providers.insert(name.to_string(), provider);
    providers.validate()?;
    commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &providers,
        &state.models,
    )?;
    Ok(())
}

/// Everything that still names `provider`: home models, project models, crew slots.
#[derive(Debug, Default, PartialEq)]
struct Users {
    home_models: Vec<String>,
    project_models: Vec<String>,
    crew_categories: Vec<String>,
}

impl Users {
    fn is_empty(&self) -> bool {
        self.home_models.is_empty()
            && self.project_models.is_empty()
            && self.crew_categories.is_empty()
    }
}

fn users_of(
    provider: &str,
    home: &ModelsConfig,
    project: Option<&ModelsConfig>,
    crew: Option<&cowboy_core::crew::CrewConfig>,
) -> Users {
    let names = |cfg: &ModelsConfig| -> Vec<String> {
        cfg.iter()
            .filter(|(r, _)| r.provider == provider)
            .map(|(r, _)| r.to_string())
            .collect()
    };
    let mut crew_categories = BTreeSet::new();
    if let Some(c) = crew {
        for (cat, ramp) in &c.crew {
            let uses = ramp.models().into_iter().any(|m| {
                cowboy_core::config::ModelRef::parse(m).is_ok_and(|r| r.provider == provider)
            });
            if uses {
                crew_categories.insert(cat.clone());
            }
        }
    }
    Users {
        home_models: names(home),
        project_models: project.map(names).unwrap_or_default(),
        crew_categories: crew_categories.into_iter().collect(),
    }
}

fn remove(name: &str, force: bool) -> Result<()> {
    let (providers_path, models_path) = paths()?;
    let state = load_model_setup_state(&providers_path, &models_path)?;
    if !state.providers.providers.contains_key(name) {
        bail!("no provider `{name}`; see `cowboy providers list`");
    }
    let project = project_models();
    let crew = cowboy_core::crew::load().ok().flatten();
    let users = users_of(name, &state.models, project.as_ref(), crew.as_ref());
    if !users.is_empty() {
        let mut why = Vec::new();
        if !users.home_models.is_empty() {
            why.push(format!("home models {}", users.home_models.join(", ")));
        }
        if !users.project_models.is_empty() {
            why.push(format!(
                "project models {}",
                users.project_models.join(", ")
            ));
        }
        if !users.crew_categories.is_empty() {
            why.push(format!(
                "crew categories {}",
                users.crew_categories.join(", ")
            ));
        }
        if !force {
            bail!(
                "provider `{name}` is still used by {} — move them first, or pass --force",
                why.join("; ")
            );
        }
        crate::ui::warn(&format!("`{name}` is used by {}", why.join("; ")));
        if !crate::prompt::confirm_destructive(&format!(
            "Remove provider `{name}` and its home models?"
        ))? {
            return Ok(());
        }
    }

    let mut providers = state.providers.clone();
    providers.providers.remove(name);
    let mut models = state.models.clone();
    models.models.remove(name);
    let cleared_default = models.default.as_ref().is_some_and(|d| d.provider == name);
    if cleared_default {
        models.default = None;
    }
    if models
        .summarizer
        .as_ref()
        .is_some_and(|d| d.provider == name)
    {
        models.summarizer = None;
    }
    commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &providers,
        &models,
    )?;
    crate::ui::ok(&format!("removed provider `{name}`"));
    if cleared_default {
        crate::ui::warn("it held your default model; pick another with `cowboy models use`");
    }
    if !users.project_models.is_empty() || !users.crew_categories.is_empty() {
        crate::ui::warn(
            "project models and crew slots naming it were left as they are; \
             `cowboy doctor` and `cowboy crew validate` will flag them",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::{ModelDef, ModelRef};

    fn def() -> ModelDef {
        serde_yaml_ng::from_str("model: x\n").unwrap()
    }

    #[test]
    fn users_of_finds_home_project_and_crew_references() {
        let mut home = ModelsConfig::default();
        home.insert(ModelRef::parse("a/m").unwrap(), def());
        home.insert(ModelRef::parse("b/m").unwrap(), def());
        let mut project = ModelsConfig::default();
        project.insert(ModelRef::parse("a/p").unwrap(), def());
        let crew: cowboy_core::crew::CrewConfig =
            serde_yaml_ng::from_str("crew:\n  general: a/m\n  deep: b/m\n  x: codex\n").unwrap();

        let u = users_of("a", &home, Some(&project), Some(&crew));
        assert_eq!(u.home_models, ["a/m"]);
        assert_eq!(u.project_models, ["a/p"]);
        assert_eq!(u.crew_categories, ["general"]);
        assert!(users_of("c", &home, Some(&project), Some(&crew)).is_empty());
    }

    #[test]
    fn auth_summary_never_prints_the_key() {
        let p = Provider::api_key("http://x/v1", "sk-very-secret");
        assert!(!auth_summary("x", &p).contains("sk-very-secret"));
        assert_eq!(
            auth_summary("x", &Provider::api_key("http://x/v1", "")),
            "none (keyless)"
        );
    }
}
