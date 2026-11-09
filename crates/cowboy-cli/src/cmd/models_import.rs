//! `cowboy models import` and `cowboy models refresh`: model definitions from a
//! provider's catalogue (`cowboy_core::model_import` decides what they contain;
//! this side asks which, shows them, and commits them under the model-config lock).

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cowboy_core::config::{
    commit_model_setup_pair, load_model_setup_state, ModelDef, ModelRef, ModelsConfig,
    ProvidersConfig,
};
use cowboy_core::model::{list_models, ModelEntry};
use cowboy_core::model_import::{plan_import, refresh as plan_refresh};

const CATALOGUE_TIMEOUT: Duration = Duration::from_secs(20);

/// Which of the new models to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Select {
    All,
    Matching(String),
    /// Ask (on a terminal).
    Ask,
}

impl Select {
    pub fn from_flags(pattern: Option<String>, all: bool) -> Self {
        match (all, pattern) {
            (true, _) => Select::All,
            (false, Some(p)) => Select::Matching(p),
            (false, None) => Select::Ask,
        }
    }
}

/// Case-insensitive match: `*` matches any run of characters; a pattern with no
/// `*` matches as a substring.
pub fn matches(pattern: &str, id: &str) -> bool {
    let (p, id) = (pattern.to_ascii_lowercase(), id.to_ascii_lowercase());
    if !p.contains('*') {
        return id.contains(&p);
    }
    let parts: Vec<&str> = p.split('*').collect();
    let mut rest = id.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// `1,3,5-7` → zero-based indices below `n`.
pub fn parse_picks(input: &str, n: usize) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in input.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<usize>()?, b.trim().parse::<usize>()?),
            None => {
                let x = part.parse::<usize>()?;
                (x, x)
            }
        };
        if a == 0 || b < a || b > n {
            bail!("{part} is not in 1–{n}");
        }
        out.extend((a - 1)..b);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

async fn catalogue(name: &str) -> Result<Vec<ModelEntry>> {
    let providers = ProvidersConfig::load_global()?;
    let p = providers
        .providers
        .get(name)
        .with_context(|| format!("no provider `{name}`; see `cowboy providers list`"))?;
    tokio::time::timeout(CATALOGUE_TIMEOUT, list_models(name, p))
        .await
        .map_err(|_| anyhow::anyhow!("`{name}`'s catalogue did not answer in 20s"))?
        .with_context(|| format!("reading `{name}`'s model catalogue"))
}

fn describe(r: &ModelRef, d: &ModelDef) -> String {
    format!(
        "{:<34} {:<46} ctx {:>8}  out {:>6}  reasoning {}",
        r.to_string(),
        d.model,
        d.context_window,
        d.max_tokens,
        d.reasoning_effort.map(|e| e.as_str()).unwrap_or("-")
    )
}

/// Import from `provider`'s catalogue. Returns how many models were saved.
pub async fn import(provider: &str, select: Select, yes: bool) -> Result<usize> {
    let entries = catalogue(provider).await?;
    let (providers_path, models_path) = (
        ProvidersConfig::global_path().context("cannot resolve home config directory")?,
        ModelsConfig::user_path().context("cannot resolve home config directory")?,
    );
    let state = load_model_setup_state(&providers_path, &models_path)?;
    let plan = plan_import(provider, &entries, &state.models);
    if plan.is_empty() {
        crate::ui::ok(&format!(
            "nothing new to import from `{provider}` ({} catalogue entries; chat models already configured)",
            entries.len()
        ));
        return Ok(0);
    }
    let interactive = std::io::stdin().is_terminal() && !crate::prompt::assume_yes();
    let chosen: Vec<(ModelRef, ModelDef)> = match select {
        Select::All => plan,
        Select::Matching(p) => plan
            .into_iter()
            .filter(|(_, d)| matches(&p, &d.model))
            .collect(),
        Select::Ask if !interactive => bail!(
            "`{provider}` offers {} new chat model(s); pass --all or --match <pattern> to choose \
             without a terminal",
            plan.len()
        ),
        Select::Ask => ask(provider, plan)?,
    };
    if chosen.is_empty() {
        println!("no models selected; nothing changed");
        return Ok(0);
    }
    println!("to import into {}:", models_path.display());
    for (r, d) in &chosen {
        println!("  {}", describe(r, d));
    }
    if !yes && !crate::prompt::confirm(&format!("Save {} model(s)?", chosen.len()), true)? {
        return Ok(0);
    }
    let mut models = state.models.clone();
    let n = chosen.len();
    let first = chosen[0].0.clone();
    for (r, d) in chosen {
        models.insert(r, d);
    }
    let made_default = models.default.is_none();
    if made_default {
        models.default = Some(first.clone());
    }
    commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &state.providers,
        &models,
    )?;
    crate::ui::ok(&format!("imported {n} model(s) from `{provider}`"));
    if made_default {
        println!("  default model: {first} (change it with `cowboy models use`)");
    }
    Ok(n)
}

fn ask(provider: &str, plan: Vec<(ModelRef, ModelDef)>) -> Result<Vec<(ModelRef, ModelDef)>> {
    loop {
        let raw = crate::prompt::line(
            &format!(
                "`{provider}` offers {} new chat model(s): import [a]ll, by [p]attern, pick from a \
                 [l]ist, or [n]one?",
                plan.len()
            ),
            Some("l"),
        )?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "a" | "all" => return Ok(plan),
            "n" | "none" => return Ok(Vec::new()),
            "p" | "pattern" => {
                let p = crate::prompt::line("Pattern (`*` is a wildcard)", None)?;
                let hits: Vec<_> = plan
                    .iter()
                    .filter(|(_, d)| matches(&p, &d.model))
                    .cloned()
                    .collect();
                if hits.is_empty() {
                    crate::ui::warn("no model matches that; try again");
                    continue;
                }
                return Ok(hits);
            }
            "l" | "list" => {
                for (i, (r, d)) in plan.iter().enumerate() {
                    println!("  {:>3}. {}", i + 1, describe(r, d));
                }
                let picks = crate::prompt::line("Numbers to import (e.g. 1,3,5-7)", None)?;
                match parse_picks(&picks, plan.len()) {
                    Ok(ix) => return Ok(ix.into_iter().map(|i| plan[i].clone()).collect()),
                    Err(e) => crate::ui::warn(&format!("{e}; try again")),
                }
            }
            _ => crate::ui::warn("answer a, p, l or n"),
        }
    }
}

/// Offer an import at the end of setting up `provider`: interactive only, never an
/// error (setup already succeeded).
pub async fn offer(provider: &str) {
    if !std::io::stdin().is_terminal() {
        println!("  import its models with `cowboy models import {provider}`");
        return;
    }
    match crate::prompt::confirm(
        &format!("Import models from `{provider}`'s catalogue now?"),
        true,
    ) {
        Ok(true) => {
            if let Err(e) = import(provider, Select::Ask, false).await {
                crate::ui::warn(&format!(
                    "import failed ({e:#}); retry with `cowboy models import {provider}`"
                ));
            }
        }
        _ => println!("  later: `cowboy models import {provider}`"),
    }
}

/// Update configured models from their providers' catalogues.
pub async fn refresh(only: Option<&str>, yes: bool) -> Result<()> {
    let providers = ProvidersConfig::load_global()?;
    let (providers_path, models_path) = (
        ProvidersConfig::global_path().context("cannot resolve home config directory")?,
        ModelsConfig::user_path().context("cannot resolve home config directory")?,
    );
    let state = load_model_setup_state(&providers_path, &models_path)?;
    let mut models = state.models.clone();
    let names: Vec<String> = match only {
        Some(n) => {
            if !providers.providers.contains_key(n) {
                bail!("no provider `{n}`; see `cowboy providers list`");
            }
            vec![n.to_string()]
        }
        None => models
            .models
            .keys()
            .filter(|p| providers.providers.contains_key(*p))
            .cloned()
            .collect(),
    };
    let mut changes = Vec::new();
    for name in &names {
        let entries = match catalogue(name).await {
            Ok(e) => e,
            Err(e) => {
                crate::ui::warn(&format!("skipped `{name}`: {e:#}"));
                continue;
            }
        };
        let (c, missing) = plan_refresh(name, &entries, &mut models);
        for m in missing {
            println!("  note: {m} is no longer in `{name}`'s catalogue (left as is)");
        }
        changes.extend(c);
    }
    if changes.is_empty() {
        crate::ui::ok("models are up to date with their catalogues");
        return Ok(());
    }
    for c in &changes {
        println!(
            "  {:<34} {:<17} {} → {}",
            c.model.to_string(),
            c.field,
            c.from,
            c.to
        );
    }
    if !yes && !crate::prompt::confirm(&format!("Apply {} change(s)?", changes.len()), true)? {
        return Ok(());
    }
    let mut bak = models_path.as_os_str().to_owned();
    bak.push(".bak");
    std::fs::copy(&models_path, &bak).context("backing up models.yaml")?;
    commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &state.providers,
        &models,
    )?;
    crate::ui::ok(&format!(
        "updated {} field(s) (backup: {})",
        changes.len(),
        std::path::Path::new(&bak).display()
    ));
    println!("  running sessions keep the old values until they switch models or restart");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_wildcards_or_substrings() {
        assert!(matches("glm", "fireworks/GLM-5p3"));
        assert!(matches("glm*", "glm-5p3"));
        assert!(!matches("glm*", "fireworks/glm-5p3"));
        assert!(matches("*glm*flash", "fireworks/glm-5p3-flash"));
        assert!(!matches("*glm*flash", "fireworks/glm-5p3"));
        assert!(matches("*", "anything"));
    }

    #[test]
    fn picks_accept_lists_and_ranges() {
        assert_eq!(parse_picks("1, 3,5-7", 9).unwrap(), [0, 2, 4, 5, 6]);
        assert_eq!(parse_picks("2,2", 3).unwrap(), [1]);
        assert!(parse_picks("0", 3).is_err());
        assert!(parse_picks("4", 3).is_err());
        assert!(parse_picks("x", 3).is_err());
    }
}
