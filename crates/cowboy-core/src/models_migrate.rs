//! `cowboy models migrate`: the one-time rewrite from the flat `models.yaml` layout
//! (bare model names, a `provider:` field on each) to models grouped by provider
//! and referenced as `provider/name`.
//!
//! Pure text-in, value-out so it is testable without a home directory; the CLI
//! owns locking, backups and confirmation. Old names map to their new reference
//! through one table built from *both* models files, because a project `default`
//! (or a crew slot) may name a model defined only at the user level.

use std::collections::BTreeMap;

use serde_yaml_ng::Value;

use crate::config::{is_legacy_models_layout, ModelDef, ModelRef, ModelsConfig};
use crate::crew::{CrewConfig, DEFAULT_MODEL};
use crate::error::{Error, Result};

/// Old bare name → its qualified reference.
pub type Renames = BTreeMap<String, ModelRef>;

fn parse(text: &str) -> Result<Value> {
    serde_yaml_ng::from_str(text).map_err(|e| Error::Invalid(e.to_string()))
}

/// The new name for an old one. Old names were often the provider-side id itself
/// (`fireworks/accounts/fireworks/models/glm-5p3`), which can no longer contain
/// `/`; the last segment (`glm-5p3`) is what anyone would call it, so that is
/// used unless another model under the same provider already claims it, in
/// which case the whole id is kept with `/` turned into `-`.
fn new_name(
    old: &str,
    taken: &std::collections::BTreeSet<(String, String)>,
    provider: &str,
) -> String {
    let old = old.trim();
    if !old.contains('/') {
        return old.to_string();
    }
    let short = old.rsplit('/').next().unwrap_or(old).trim();
    if !short.is_empty() && !taken.contains(&(provider.to_string(), short.to_string())) {
        short.to_string()
    } else {
        old.replace('/', "-")
    }
}

/// The renames a legacy models file implies (empty when it is already grouped).
pub fn legacy_renames(text: &str) -> Result<Renames> {
    let v = parse(text)?;
    let mut out = Renames::new();
    if !is_legacy_models_layout(&v) {
        return Ok(out);
    }
    let Some(models) = v.get("models").and_then(Value::as_mapping) else {
        return Ok(out);
    };
    let mut entries = Vec::new();
    for (k, entry) in models {
        let name = k
            .as_str()
            .ok_or_else(|| Error::Invalid("a model name in models.yaml is not a string".into()))?;
        let provider = entry
            .get("provider")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "model `{name}` has no `provider:`; add one, then migrate"
                ))
            })?;
        entries.push((name.to_string(), provider.to_string()));
    }
    // Names without a `/` keep themselves, so they claim their slot first.
    let mut taken: std::collections::BTreeSet<(String, String)> = entries
        .iter()
        .filter(|(n, _)| !n.contains('/'))
        .map(|(n, p)| (p.clone(), n.trim().to_string()))
        .collect();
    for (name, provider) in &entries {
        let new = new_name(name, &taken, provider);
        taken.insert((provider.clone(), new.clone()));
        out.insert(name.clone(), ModelRef::new(provider.clone(), new)?);
    }
    Ok(out)
}

/// Rewrite one models file. `None` when it is already in the grouped layout.
/// `renames` must cover every bare name the file's `default`/`summarizer` uses —
/// pass the union of [`legacy_renames`] over the user and project files.
pub fn migrate_models_text(text: &str, renames: &Renames) -> Result<Option<ModelsConfig>> {
    let v = parse(text)?;
    if !is_legacy_models_layout(&v) {
        return Ok(None);
    }
    // This file's own entries are named by this file's table; `renames` (both
    // files) is only for what `default`/`summarizer` point at.
    let own = legacy_renames(text)?;
    let mut out = ModelsConfig::default();
    if let Some(n) = v.get("version").and_then(Value::as_u64) {
        out.version = n as u32;
    }
    if let Some(models) = v.get("models").and_then(Value::as_mapping) {
        for (k, entry) in models {
            let name = k.as_str().unwrap_or_default();
            let mut entry = entry.clone();
            if let Some(m) = entry.as_mapping_mut() {
                m.remove("provider");
            }
            let r = own
                .get(name)
                .cloned()
                .ok_or_else(|| Error::Invalid(format!("model `{name}` has no `provider:`")))?;
            let def: ModelDef = serde_yaml_ng::from_value(entry)
                .map_err(|e| Error::Invalid(format!("model `{name}`: {e}")))?;
            out.insert(r, def);
        }
    }
    let reference = |key: &str| -> Result<Option<ModelRef>> {
        match v.get(key) {
            None | Some(Value::Null) => Ok(None),
            // An old name wins even when it looks qualified: `a/b/c` was a name.
            Some(Value::String(s)) => match own.get(s).or_else(|| renames.get(s)) {
                Some(r) => Ok(Some(r.clone())),
                None if s.contains('/') => ModelRef::parse(s).map(Some),
                None => Err(Error::Invalid(format!(
                    "`{key}: {s}` names no model in either models.yaml; fix it by hand"
                ))),
            },
            Some(_) => Err(Error::Invalid(format!("`{key}` must be a model name"))),
        }
    };
    out.default = reference("default")?;
    out.summarizer = reference("summarizer")?;
    Ok(Some(out))
}

/// Rewrite a roster's bare model slots to `provider/name`. Slots that are
/// `<default>`, already qualified, or a harness are left alone. Returns whether
/// anything changed. A bare slot that is none of those is an error rather than a
/// guess.
pub fn migrate_crew(
    cfg: &mut CrewConfig,
    renames: &Renames,
    is_harness: impl Fn(&str) -> bool,
) -> Result<bool> {
    let mut changed = false;
    for (cat, ramp) in cfg.crew.iter_mut() {
        for slot in ramp.models_mut() {
            // An old model name first: it may itself contain `/`.
            if let Some(r) = renames.get(slot.as_str()) {
                *slot = r.to_string();
                changed = true;
                continue;
            }
            if slot == DEFAULT_MODEL || ModelRef::parse(slot).is_ok() || is_harness(slot) {
                continue;
            }
            return Err(Error::Invalid(format!(
                "crew.yaml category `{cat}` names `{slot}`, which is neither a model nor a \
                 harness; fix it by hand"
            )));
        }
    }
    Ok(changed)
}

/// `before` (a crew.yaml) with each slot value renamed in place, so the user's
/// comments and layout survive. Only `key: value` lines whose value is an old
/// name are touched; the caller checks the result parses to the migrated roster
/// and falls back to a plain re-serialization if it does not.
pub fn rename_crew_text(before: &str, renames: &Renames) -> String {
    let mut out = String::with_capacity(before.len());
    for line in before.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        let (code, comment) = match body.find(" #") {
            Some(i) => body.split_at(i),
            None => (body, ""),
        };
        let renamed = code.rfind(": ").and_then(|i| {
            let (head, value) = code.split_at(i + 2);
            let v = value.trim();
            let unquoted = v.trim_matches(|c| c == '"' || c == '\'');
            renames.get(unquoted).map(|r| {
                let pad = &value[value.trim_end().len()..];
                format!("{head}{r}{pad}")
            })
        });
        match renamed {
            Some(code) => {
                out.push_str(&code);
                out.push_str(comment);
                out.push_str(eol);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// True when a roster still has bare slots that are not harnesses — what the
/// worker and `doctor` check to point at migration.
pub fn crew_needs_migration(cfg: &CrewConfig, is_harness: impl Fn(&str) -> bool) -> bool {
    cfg.crew.values().any(|ramp| {
        ramp.models()
            .into_iter()
            .any(|m| m != DEFAULT_MODEL && ModelRef::parse(m).is_err() && !is_harness(m))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "version: 1\ndefault: glm\nsummarizer: mini\nmodels:\n  \
        glm:\n    provider: bifrost\n    model: zai/glm-4.7\n    context_window: 200000\n  \
        mini:\n    provider: openai\n    model: gpt-mini\n  \
        odd/name:\n    provider: bifrost\n    model: x\n";
    const PROJECT: &str = "default: mini\nmodels:\n  local:\n    provider: bifrost\n    model: l\n";

    fn renames() -> Renames {
        let mut r = legacy_renames(USER).unwrap();
        r.extend(legacy_renames(PROJECT).unwrap());
        r
    }

    #[test]
    fn a_flat_file_is_grouped_by_provider_with_qualified_references() {
        let cfg = migrate_models_text(USER, &renames()).unwrap().unwrap();
        assert_eq!(cfg.default.as_ref().unwrap().to_string(), "bifrost/glm");
        assert_eq!(cfg.summarizer.as_ref().unwrap().to_string(), "openai/mini");
        let glm = cfg.models["bifrost"]["glm"].clone();
        assert_eq!(glm.model, "zai/glm-4.7");
        assert_eq!(glm.context_window, 200_000);
        assert!(
            cfg.models["bifrost"].contains_key("name"),
            "an id-like name keeps its tail"
        );
        // The result is a valid new-layout file.
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let back: ModelsConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn a_project_default_can_name_a_user_model() {
        let cfg = migrate_models_text(PROJECT, &renames()).unwrap().unwrap();
        assert_eq!(cfg.default.unwrap().to_string(), "openai/mini");
        assert!(cfg.models["bifrost"].contains_key("local"));
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let cfg = migrate_models_text(USER, &renames()).unwrap().unwrap();
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        assert!(migrate_models_text(&yaml, &renames()).unwrap().is_none());
        assert!(legacy_renames(&yaml).unwrap().is_empty());
    }

    #[test]
    fn a_dangling_default_is_an_error_not_a_guess() {
        let text = "default: ghost\nmodels:\n  a:\n    provider: p\n    model: x\n";
        let err = migrate_models_text(text, &legacy_renames(text).unwrap()).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn crew_slots_are_qualified_and_harnesses_left_alone() {
        let mut cfg: CrewConfig = serde_yaml_ng::from_str(
            "crew:\n  general: glm\n  deep:\n    tiny: mini\n    deep: <default>\n  \
             review: codex\n  x: bifrost/already\n",
        )
        .unwrap();
        let harness = |n: &str| n == "codex";
        assert!(crew_needs_migration(&cfg, harness));
        assert!(migrate_crew(&mut cfg, &renames(), harness).unwrap());
        assert!(!crew_needs_migration(&cfg, harness));
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        assert!(yaml.contains("general: bifrost/glm"), "{yaml}");
        assert!(yaml.contains("tiny: openai/mini"), "{yaml}");
        assert!(yaml.contains("deep: <default>"), "{yaml}");
        assert!(yaml.contains("review: codex"), "{yaml}");
        // Second pass: nothing left to do.
        assert!(!migrate_crew(&mut cfg, &renames(), harness).unwrap());
    }

    #[test]
    fn id_like_names_shorten_unless_that_collides() {
        let text = "default: fireworks/accounts/m/glm\nmodels:\n  \
            fireworks/accounts/m/glm:\n    provider: d\n    model: fireworks/accounts/m/glm\n  \
            other/glm:\n    provider: d\n    model: o\n  \
            kimi:\n    provider: d\n    model: k\n  \
            x/kimi:\n    provider: d\n    model: k2\n";
        let r = legacy_renames(text).unwrap();
        // The bare `kimi` keeps its name, so the id-like one cannot take it.
        assert_eq!(r["kimi"].to_string(), "d/kimi");
        assert_eq!(r["x/kimi"].to_string(), "d/x-kimi");
        // Exactly one of the two `glm` tails wins; the other keeps its full id.
        let glm: Vec<String> = ["fireworks/accounts/m/glm", "other/glm"]
            .iter()
            .map(|k| r[*k].name.clone())
            .collect();
        assert!(glm.contains(&"glm".to_string()), "{glm:?}");
        // An old default that *looks* qualified is still an old name.
        let cfg = migrate_models_text(text, &r).unwrap().unwrap();
        assert_eq!(cfg.default.unwrap(), r["fireworks/accounts/m/glm"]);
    }

    #[test]
    fn crew_text_is_renamed_in_place_keeping_comments() {
        let text = "version: 1\n# my roster\ncrew:\n  general: glm # cheap\n  deep:\n    \
                    tiny: \"fireworks/accounts/m/glm\"\n    deep: <default>\n";
        let mut r = Renames::new();
        r.insert("glm".into(), ModelRef::parse("d/glm").unwrap());
        r.insert(
            "fireworks/accounts/m/glm".into(),
            ModelRef::parse("d/g2").unwrap(),
        );
        let out = rename_crew_text(text, &r);
        assert!(out.contains("# my roster\n"), "{out}");
        assert!(out.contains("  general: d/glm # cheap\n"), "{out}");
        assert!(out.contains("    tiny: d/g2\n"), "{out}");
        assert!(out.contains("deep: <default>"), "{out}");
    }

    #[test]
    fn an_unknown_bare_crew_slot_is_an_error() {
        let mut cfg: CrewConfig = serde_yaml_ng::from_str("crew:\n  general: nope\n").unwrap();
        assert!(migrate_crew(&mut cfg, &renames(), |_| false).is_err());
    }
}
