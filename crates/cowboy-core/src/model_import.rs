//! Model definitions from a provider's catalogue: what `cowboy models import`
//! saves and what `cowboy models refresh` corrects.
//!
//! A catalogue entry says what the endpoint serves (its id, sometimes its context
//! window and reasoning levels); Cowboy's shipped table ([`crate::model_defaults`])
//! knows output caps, sampling and prices for known ids. The catalogue wins where
//! it speaks, because it describes this deployment; the table fills the rest.

use std::collections::BTreeSet;

use crate::config::{ModelDef, ModelRef, ModelsConfig, ReasoningEffort};
use crate::model::ModelEntry;
use crate::model_defaults;

/// A catalogue's reasoning level as one of Cowboy's. Levels above `high`
/// (`xhigh`, `max`, `ultra`) become `high`; `none`/`off` mean no hint.
pub fn map_reasoning(level: &str) -> Option<ReasoningEffort> {
    match level.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" | "xhigh" | "max" | "ultra" => Some(ReasoningEffort::High),
        _ => None,
    }
}

/// Whether `effort` is a level the entry accepts (always true when the catalogue
/// doesn't list levels).
fn accepts(e: &ModelEntry, effort: ReasoningEffort) -> bool {
    e.supported_reasoning.is_empty()
        || e.supported_reasoning
            .iter()
            .any(|l| l.eq_ignore_ascii_case(effort.as_str()))
}

/// The reasoning effort to save for an entry: the catalogue's default, else the
/// shipped one — but never a level the catalogue says the model refuses.
fn reasoning_for(e: &ModelEntry, shipped: Option<ReasoningEffort>) -> Option<ReasoningEffort> {
    let pick = e
        .default_reasoning
        .as_deref()
        .and_then(map_reasoning)
        .or(shipped);
    match pick {
        Some(r) if accepts(e, r) => Some(r),
        // A shipped level the model refuses: its lowest accepted level instead.
        Some(_) => e.supported_reasoning.iter().find_map(|l| map_reasoning(l)),
        None => None,
    }
}

/// A model definition for a catalogue entry.
pub fn def_from_entry(e: &ModelEntry) -> ModelDef {
    let d = model_defaults::lookup(&e.id);
    let context_window = e.context_window.unwrap_or(d.context_window);
    ModelDef {
        model: e.id.clone(),
        temperature: d.temperature,
        // An output cap larger than the window could never be honoured.
        max_tokens: d.max_tokens.min(context_window / 2).max(1),
        context_window,
        reasoning_effort: reasoning_for(e, d.reasoning_effort),
        top_p: None,
        stop: Vec::new(),
        extra: Default::default(),
        headers: Default::default(),
        input_cost_per_mtok: d.input_cost_per_mtok,
        output_cost_per_mtok: d.output_cost_per_mtok,
        cached_input_cost_per_mtok: d.cached_input_cost_per_mtok,
        anthropic_cache: false,
        stream_idle_timeout_seconds: None,
    }
}

/// A usable model name from an id: its last `/` segment, trimmed.
fn short_name(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).trim().to_string()
}

/// The models an import would add for `provider`: chat models from `entries` that
/// the user has not configured on that provider already (same provider-side id),
/// each named `provider/<last segment of its id>` — or the whole id, `/` turned
/// into `-`, when that name is taken.
pub fn plan_import(
    provider: &str,
    entries: &[ModelEntry],
    existing: &ModelsConfig,
) -> Vec<(ModelRef, ModelDef)> {
    let group = existing.models.get(provider);
    let configured: BTreeSet<&str> = group
        .map(|g| g.values().map(|d| d.model.as_str()).collect())
        .unwrap_or_default();
    let mut taken: BTreeSet<String> = group
        .map(|g| g.keys().cloned().collect())
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for e in entries {
        if !model_defaults::is_chat(&e.id)
            || configured.contains(e.id.as_str())
            || !seen.insert(e.id.clone())
        {
            continue;
        }
        let mut name = short_name(&e.id);
        if name.is_empty() || taken.contains(&name) {
            name = e.id.trim().replace('/', "-");
        }
        let base = name.clone();
        let mut n = 2;
        while taken.contains(&name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let Ok(r) = ModelRef::new(provider, name.clone()) else {
            continue;
        };
        taken.insert(name);
        out.push((r, def_from_entry(e)));
    }
    out
}

/// One field `refresh` would change.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub model: ModelRef,
    pub field: &'static str,
    pub from: String,
    pub to: String,
}

/// Corrections from a fresh catalogue for the models configured on `provider` in
/// `cfg`: the context window when the catalogue reports a different one, and the
/// reasoning effort only when the configured level is one the model refuses. Your
/// other settings (prices, temperature, output cap) are yours and stay. Returns
/// the changes and the models the catalogue no longer lists.
pub fn refresh(
    provider: &str,
    entries: &[ModelEntry],
    cfg: &mut ModelsConfig,
) -> (Vec<Change>, Vec<ModelRef>) {
    let mut changes = Vec::new();
    let mut missing = Vec::new();
    let Some(group) = cfg.models.get_mut(provider) else {
        return (changes, missing);
    };
    for (name, def) in group.iter_mut() {
        let r = ModelRef {
            provider: provider.to_string(),
            name: name.clone(),
        };
        let Some(e) = entries.iter().find(|e| e.id == def.model) else {
            missing.push(r);
            continue;
        };
        if let Some(w) = e.context_window.filter(|w| *w != def.context_window) {
            changes.push(Change {
                model: r.clone(),
                field: "context_window",
                from: def.context_window.to_string(),
                to: w.to_string(),
            });
            def.context_window = w;
            if def.max_tokens > w / 2 {
                let cap = (w / 2).max(1);
                changes.push(Change {
                    model: r.clone(),
                    field: "max_tokens",
                    from: def.max_tokens.to_string(),
                    to: cap.to_string(),
                });
                def.max_tokens = cap;
            }
        }
        if let Some(cur) = def.reasoning_effort.filter(|c| !accepts(e, *c)) {
            let to = reasoning_for(e, None);
            changes.push(Change {
                model: r.clone(),
                field: "reasoning_effort",
                from: cur.as_str().to_string(),
                to: to
                    .map(|t| t.as_str().to_string())
                    .unwrap_or_else(|| "none".into()),
            });
            def.reasoning_effort = to;
        }
    }
    (changes, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chatgpt(slug: &str, window: u32, default: &str, levels: &[&str]) -> ModelEntry {
        ModelEntry {
            context_window: Some(window),
            default_reasoning: Some(default.into()),
            supported_reasoning: levels.iter().map(|s| s.to_string()).collect(),
            ..ModelEntry::bare(slug)
        }
    }

    #[test]
    fn the_catalogue_wins_where_it_speaks() {
        let e = chatgpt(
            "gpt-6.1-sol",
            500_000,
            "low",
            &["low", "medium", "high", "xhigh"],
        );
        let d = def_from_entry(&e);
        assert_eq!(d.context_window, 500_000);
        assert_eq!(d.reasoning_effort, Some(ReasoningEffort::Low));
        assert!(d.max_tokens <= 250_000);

        // A Bifrost-shape entry: only the window; the rest from the shipped table.
        let mut b = ModelEntry::bare("anthropic/claude-sonnet-4-6");
        b.context_window = Some(1_000_000);
        let d = def_from_entry(&b);
        assert_eq!(d.context_window, 1_000_000);
        assert!(d.input_cost_per_mtok.is_some(), "shipped prices fill in");
    }

    #[test]
    fn a_level_the_model_refuses_is_never_saved() {
        // The catalogue's default is above Cowboy's range: mapped to high.
        let e = chatgpt("x", 1000, "xhigh", &["low", "xhigh"]);
        // `high` itself is not listed, so the lowest accepted level is used.
        assert_eq!(
            def_from_entry(&e).reasoning_effort,
            Some(ReasoningEffort::Low)
        );
        assert_eq!(map_reasoning("ultra"), Some(ReasoningEffort::High));
        assert_eq!(map_reasoning("none"), None);
    }

    #[test]
    fn an_unknown_model_without_metadata_gets_the_generic_defaults() {
        let d = def_from_entry(&ModelEntry::bare("someone/new-model-9"));
        assert!(d.context_window > 0 && d.max_tokens > 0);
    }

    #[test]
    fn import_names_by_last_segment_skips_configured_and_non_chat() {
        let mut existing = ModelsConfig::default();
        let glm: ModelDef = serde_yaml_ng::from_str("model: fireworks/glm-5p3\n").unwrap();
        existing.insert(ModelRef::parse("bifrost/glm-5p3").unwrap(), glm);
        let entries = [
            ModelEntry::bare("fireworks/glm-5p3"),    // configured: skipped
            ModelEntry::bare("cerebras/zai-glm-4.7"), // new
            ModelEntry::bare("other/glm-5p3"),        // name taken: full id
            ModelEntry::bare("openai/text-embedding-3"), // not chat
            ModelEntry::bare("cerebras/zai-glm-4.7"), // duplicate
        ];
        let plan = plan_import("bifrost", &entries, &existing);
        let names: Vec<String> = plan.iter().map(|(r, _)| r.to_string()).collect();
        assert_eq!(names, ["bifrost/zai-glm-4.7", "bifrost/other-glm-5p3"]);
    }

    #[test]
    fn refresh_fixes_windows_and_refused_levels_only() {
        let mut cfg = ModelsConfig::default();
        let def: ModelDef = serde_yaml_ng::from_str(
            "model: gpt-6.1-sol\ncontext_window: 131072\nmax_tokens: 8192\n\
             reasoning_effort: minimal\ntemperature: 0.7\n",
        )
        .unwrap();
        cfg.insert(ModelRef::parse("chatgpt/sol").unwrap(), def);
        let gone: ModelDef = serde_yaml_ng::from_str("model: gpt-4\n").unwrap();
        cfg.insert(ModelRef::parse("chatgpt/old").unwrap(), gone);
        let entries = [chatgpt("gpt-6.1-sol", 500_000, "low", &["low", "medium"])];

        let (changes, missing) = refresh("chatgpt", &entries, &mut cfg);
        let fields: Vec<_> = changes.iter().map(|c| (c.field, c.to.as_str())).collect();
        assert_eq!(
            fields,
            [("context_window", "500000"), ("reasoning_effort", "low")]
        );
        assert_eq!(missing, [ModelRef::parse("chatgpt/old").unwrap()]);
        let sol = cfg.get(&ModelRef::parse("chatgpt/sol").unwrap()).unwrap();
        assert_eq!(
            (sol.max_tokens, sol.temperature),
            (8192, 0.7),
            "user settings kept"
        );
        // Up to date: nothing to do.
        assert!(refresh("chatgpt", &entries, &mut cfg).0.is_empty());
    }
}
