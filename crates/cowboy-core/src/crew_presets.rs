//! Built-in crews. A preset is a *shape* — which kind of model each category and
//! effort gets — filled from the models a provider actually lists, so it keeps
//! working when the provider ships a new generation.

use std::collections::BTreeMap;

use crate::config::ModelRef;
use crate::crew::{CrewDef, Effort, Ramp};
use crate::model::ModelEntry;

/// The four ChatGPT model families, weakest to strongest, as the account's
/// catalogue describes them.
pub const FAMILIES: [&str; 4] = ["luna", "terra", "sol", "astra"];

/// The version in `gpt-<version>-<family>` (`6.1` → `[6, 1]`), for that family.
fn version_of(id: &str, family: &str) -> Option<Vec<u32>> {
    let rest = id
        .strip_prefix("gpt-")?
        .strip_suffix(&format!("-{family}"))?;
    rest.split('.').map(|p| p.parse().ok()).collect()
}

/// The newest model of each family the catalogue lists (`None` for a family it
/// lacks).
pub fn newest_by_family(entries: &[ModelEntry]) -> BTreeMap<&'static str, Option<String>> {
    FAMILIES
        .iter()
        .map(|f| {
            let best = entries
                .iter()
                .filter_map(|e| version_of(&e.id, f).map(|v| (v, &e.id)))
                .max_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, id)| id.clone());
            (*f, best)
        })
        .collect()
}

/// Which family stands in for a missing one: the nearest stronger, then the
/// nearest weaker.
fn fallback_order(family: &str) -> Vec<&'static str> {
    let i = FAMILIES.iter().position(|f| *f == family).unwrap_or(0);
    let mut order = vec![FAMILIES[i]];
    order.extend(FAMILIES[i + 1..].iter().copied());
    order.extend(FAMILIES[..i].iter().rev().copied());
    order
}

/// The ChatGPT preset's model per role (family → provider-side id), and a note
/// for each family filled by a stand-in. `None` when the catalogue has no model
/// of any family.
pub fn chatgpt_models(
    entries: &[ModelEntry],
) -> Option<(BTreeMap<&'static str, String>, Vec<String>)> {
    let newest = newest_by_family(entries);
    let mut out = BTreeMap::new();
    let mut notes = Vec::new();
    for f in FAMILIES {
        let pick = fallback_order(f)
            .into_iter()
            .find_map(|g| newest[g].clone().map(|id| (g, id)));
        let (used, id) = pick?;
        if used != f {
            notes.push(format!(
                "no {f} model in your catalogue; using {id} ({used}) for its slots"
            ));
        }
        out.insert(f, id);
    }
    Some((out, notes))
}

/// The ChatGPT crew's shape over four model references (one per family): fast
/// work on luna stepping up through terra (the Codex model) and sol (workhorse,
/// and the foreman) to astra (frontier) for the hardest; debugging, e2e and review
/// start stronger.
pub fn chatgpt_crew(refs: &BTreeMap<&'static str, ModelRef>) -> CrewDef {
    let m = |f: &str| refs[f].to_string();
    let ramp =
        |steps: &[(Effort, &str)]| Ramp::Steps(steps.iter().map(|(e, f)| (*e, m(f))).collect());
    use Effort::*;
    let light = ramp(&[(Tiny, "luna"), (Large, "terra"), (Deep, "sol")]);
    let build = ramp(&[
        (Tiny, "luna"),
        (Small, "terra"),
        (Medium, "sol"),
        (Deep, "astra"),
    ]);
    let strong = ramp(&[(Tiny, "terra"), (Small, "sol"), (Deep, "astra")]);
    let mut crew = BTreeMap::new();
    crew.insert("exploration".to_string(), light);
    crew.insert(
        "docs".to_string(),
        ramp(&[(Tiny, "luna"), (Medium, "terra"), (Deep, "sol")]),
    );
    crew.insert(
        "tests".to_string(),
        ramp(&[(Tiny, "luna"), (Small, "terra"), (Large, "sol")]),
    );
    for c in ["backend", "frontend", "refactor", "general"] {
        crew.insert(c.to_string(), build.clone());
    }
    crew.insert("debugging".to_string(), strong.clone());
    crew.insert(
        "e2e".to_string(),
        ramp(&[(Tiny, "terra"), (Medium, "sol"), (Deep, "astra")]),
    );
    crew.insert(
        "review".to_string(),
        ramp(&[(Tiny, "terra"), (Small, "sol"), (Large, "astra")]),
    );
    CrewDef {
        foreman: Some(refs["sol"].clone()),
        crew,
        ..CrewDef::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(ids: &[&str]) -> Vec<ModelEntry> {
        ids.iter().map(|i| ModelEntry::bare(*i)).collect()
    }

    #[test]
    fn the_newest_of_each_family_is_picked() {
        // The catalogue recorded from a real Plus account.
        let c = cat(&[
            "gpt-6.1-sol",
            "gpt-6-astra",
            "gpt-6.1-terra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-daybreak-blue-latest",
        ]);
        let (m, notes) = chatgpt_models(&c).unwrap();
        assert_eq!(m["luna"], "gpt-6-luna");
        assert_eq!(m["terra"], "gpt-6.1-terra");
        assert_eq!(m["sol"], "gpt-6.1-sol");
        assert_eq!(m["astra"], "gpt-6-astra");
        assert!(notes.is_empty());
        // 6.10 is newer than 6.9 (versions compare as numbers, not text).
        let (m, _) = chatgpt_models(&cat(&["gpt-6.9-sol", "gpt-6.10-sol"])).unwrap();
        assert_eq!(m["sol"], "gpt-6.10-sol");
    }

    #[test]
    fn a_missing_family_takes_the_nearest_stronger_then_weaker() {
        let (m, notes) = chatgpt_models(&cat(&["gpt-6-luna", "gpt-6-sol"])).unwrap();
        assert_eq!(m["terra"], "gpt-6-sol", "terra → the stronger sol");
        assert_eq!(m["astra"], "gpt-6-sol", "astra → the weaker sol");
        assert_eq!(notes.len(), 2);
        assert!(chatgpt_models(&cat(&["gpt-4o", "o3"])).is_none());
    }

    #[test]
    fn the_crew_routes_every_category_and_leads_with_sol() {
        let refs: BTreeMap<&'static str, ModelRef> = FAMILIES
            .iter()
            .map(|f| (*f, ModelRef::new("chatgpt", format!("gpt-{f}")).unwrap()))
            .collect();
        let def = chatgpt_crew(&refs);
        assert_eq!(def.foreman.unwrap().to_string(), "chatgpt/gpt-sol");
        assert!(def.crew.contains_key("general"));
        let review = &def.crew["review"];
        assert_eq!(review.pick(Effort::Deep), Some("chatgpt/gpt-astra"));
        assert_eq!(
            def.crew["exploration"].pick(Effort::Tiny),
            Some("chatgpt/gpt-luna")
        );
    }
}
