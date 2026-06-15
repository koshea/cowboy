//! Crew Roster: route delegated work to model profiles by *category* + *effort*.
//!
//! The planner (foreman) delegates work by **kind** — a category (the sort of
//! work) and an effort level (how hard) — and Cowboy resolves that to a model
//! from the user's roster. The planner never names a model directly; routing is
//! user-owned policy.
//!
//! The roster is an **effort ramp** per category: a model is assigned a *floor*
//! effort and handles that level and everything above it, until a higher floor
//! takes over. So a category can be one model for all efforts (a scalar), a
//! couple of breakpoints, or all five spelled out.
//!
//! Quotas, rate limits, spend, and backend failover belong to the LLM gateway —
//! NOT here. This module only maps work → a logical model name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Task complexity, lowest to highest. Declaration order IS the rank (derive Ord
/// relies on it), and it doubles as the floor key in an effort ramp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Tiny,
    Small,
    Medium,
    Large,
    Deep,
}

impl Effort {
    /// Every level, lowest first.
    pub fn all() -> [Effort; 5] {
        [
            Effort::Tiny,
            Effort::Small,
            Effort::Medium,
            Effort::Large,
            Effort::Deep,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Tiny => "tiny",
            Effort::Small => "small",
            Effort::Medium => "medium",
            Effort::Large => "large",
            Effort::Deep => "deep",
        }
    }

    /// Parse a level name; unknown input is the caller's problem (returns None).
    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tiny" => Some(Effort::Tiny),
            "small" => Some(Effort::Small),
            "medium" => Some(Effort::Medium),
            "large" => Some(Effort::Large),
            "deep" => Some(Effort::Deep),
            _ => None,
        }
    }
}

/// The default effort when a request omits or mis-spells one.
pub const DEFAULT_EFFORT: Effort = Effort::Medium;
/// The catch-all category every roster must define.
pub const GENERAL: &str = "general";

/// A per-category effort ramp: either a single model for all efforts, or sparse
/// floors (effort → model) that fill upward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Ramp {
    /// One model handles every effort.
    Single(String),
    /// Floors: each model covers from its effort upward to the next floor.
    Steps(BTreeMap<Effort, String>),
}

impl Ramp {
    /// The model for a requested effort, applying the ramp rule:
    /// the highest floor ≤ `effort`, else (below all floors) the lowest floor.
    /// Returns None only for an empty `Steps` map.
    pub fn pick(&self, effort: Effort) -> Option<&str> {
        match self {
            Ramp::Single(m) => Some(m.as_str()),
            Ramp::Steps(steps) => {
                // steps is sorted by Effort (BTreeMap key order == rank).
                let at_or_below = steps.range(..=effort).next_back().map(|(_, m)| m.as_str());
                at_or_below.or_else(|| steps.values().next().map(String::as_str))
            }
        }
    }

    /// Every model named in this ramp (for validation).
    fn models(&self) -> Vec<&str> {
        match self {
            Ramp::Single(m) => vec![m.as_str()],
            Ramp::Steps(s) => s.values().map(String::as_str).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Ramp::Steps(s) if s.is_empty())
    }
}

/// The foreman: the main session model and whether it may delegate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Planner {
    pub model: String,
    #[serde(default = "default_true")]
    pub may_delegate: bool,
}

/// Delegation limits. These are *throughput / safety* knobs, not quota or
/// budget controls (the gateway owns those).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    /// Local fan-out hint: how many workers to run at once (the gateway is the
    /// real backpressure). Not a quota.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
    /// How deep delegation may nest (planner = depth 0).
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Whether a worker may itself delegate (off by default).
    #[serde(default)]
    pub allow_recursive_delegation: bool,
}

impl Default for Delegation {
    fn default() -> Self {
        Self {
            max_parallel: default_max_parallel(),
            max_depth: default_max_depth(),
            allow_recursive_delegation: false,
        }
    }
}

/// The crew roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub planner: Planner,
    #[serde(default)]
    pub crew: BTreeMap<String, Ramp>,
    #[serde(default)]
    pub delegation: Delegation,
}

/// Which roster layer produced the resolved model (for logging / display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveVia {
    /// The requested category's ramp.
    Category,
    /// The `general` fallback ramp.
    General,
    /// The planner model (no category/general matched).
    Planner,
}

/// The outcome of resolving a (category, effort) request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub model: String,
    pub via: ResolveVia,
    /// True when the request didn't resolve through its own category ramp.
    pub fell_back: bool,
}

impl CrewConfig {
    /// Resolve a delegation request to a model name. Total: always returns a
    /// model (worst case the planner model). Unknown categories fall to
    /// `general`, then to the planner.
    pub fn resolve(&self, category: &str, effort: Effort) -> Resolved {
        if let Some(ramp) = self.crew.get(category) {
            if let Some(model) = ramp.pick(effort) {
                return Resolved {
                    model: model.to_string(),
                    via: ResolveVia::Category,
                    fell_back: false,
                };
            }
        }
        if let Some(ramp) = self.crew.get(GENERAL) {
            if let Some(model) = ramp.pick(effort) {
                return Resolved {
                    model: model.to_string(),
                    via: ResolveVia::General,
                    fell_back: true,
                };
            }
        }
        Resolved {
            model: self.planner.model.clone(),
            via: ResolveVia::Planner,
            fell_back: true,
        }
    }

    /// The effective model for each effort level of a category (ramps expanded),
    /// for display (`crew list`) and the TUI grid.
    pub fn expanded(&self, category: &str) -> Vec<(Effort, String)> {
        Effort::all()
            .into_iter()
            .map(|e| (e, self.resolve(category, e).model))
            .collect()
    }

    /// Validate the roster against the set of known model names.
    pub fn validate(&self, available: &BTreeSet<String>) -> Result<()> {
        let known = |m: &str| -> Result<()> {
            if available.contains(m) {
                Ok(())
            } else {
                Err(Error::Invalid(format!(
                    "crew references unknown model `{m}` (not in models.yaml)"
                )))
            }
        };
        known(&self.planner.model)?;
        if !self.crew.contains_key(GENERAL) {
            return Err(Error::Invalid(
                "crew must define a `general` category (the cross-category fallback)".into(),
            ));
        }
        for (cat, ramp) in &self.crew {
            if ramp.is_empty() {
                return Err(Error::Invalid(format!(
                    "crew category `{cat}` has no models"
                )));
            }
            for m in ramp.models() {
                known(m)?;
            }
        }
        if self.delegation.max_parallel == 0 {
            return Err(Error::Invalid(
                "delegation.max_parallel must be >= 1".into(),
            ));
        }
        Ok(())
    }
}

/// The home roster file (`~/.config/cowboy/crew.yaml`). Host-owned, like
/// `models.yaml`/`providers.yaml`; never mounted into the container.
pub fn path() -> Option<PathBuf> {
    crate::config::global_config_dir().map(|d| d.join("crew.yaml"))
}

/// Load the roster, or `None` if it doesn't exist yet.
pub fn load() -> Result<Option<CrewConfig>> {
    let Some(p) = path() else { return Ok(None) };
    if !p.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&p).map_err(|e| Error::Invalid(e.to_string()))?;
    serde_yaml_ng::from_str(&text)
        .map(Some)
        .map_err(|e| Error::Invalid(format!("parsing crew.yaml: {e}")))
}

/// Write the roster (creates `~/.config/cowboy/`; atomic temp+rename).
pub fn save(cfg: &CrewConfig) -> Result<()> {
    let p = path().ok_or_else(|| Error::Invalid("cannot resolve home config dir".into()))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    let yaml = serde_yaml_ng::to_string(cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = p.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

/// A sensible default roster built from three price tiers (cheapest / mid /
/// strongest model names, chosen by the caller from the catalogue). Emits ramps,
/// not full grids, so it's readable and easy to tweak.
pub fn default_with_tiers(cheap: &str, standard: &str, premium: &str) -> CrewConfig {
    // (category, [(floor, tier)]) — sparse ramps.
    let ramp = |steps: &[(Effort, &str)]| -> Ramp {
        Ramp::Steps(
            steps
                .iter()
                .map(|(e, m)| (*e, m.to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    let mut crew = BTreeMap::new();
    // economy work: cheap until it gets hard.
    let economy = ramp(&[
        (Effort::Tiny, cheap),
        (Effort::Large, standard),
        (Effort::Deep, premium),
    ]);
    // implementation: cheap small stuff, standard middle, premium when large.
    let build = ramp(&[
        (Effort::Tiny, cheap),
        (Effort::Small, standard),
        (Effort::Large, premium),
    ]);
    // review/e2e: bias to stronger models.
    let strong = ramp(&[(Effort::Tiny, standard), (Effort::Medium, premium)]);

    crew.insert("exploration".into(), economy.clone());
    crew.insert("docs".into(), economy.clone());
    crew.insert("tests".into(), economy.clone());
    crew.insert("backend".into(), build.clone());
    crew.insert("frontend".into(), build.clone());
    crew.insert("refactor".into(), build.clone());
    crew.insert("debugging".into(), build.clone());
    crew.insert("review".into(), strong.clone());
    crew.insert("e2e".into(), strong);
    crew.insert(GENERAL.into(), build);

    CrewConfig {
        version: 1,
        planner: Planner {
            model: premium.to_string(),
            may_delegate: true,
        },
        crew,
        delegation: Delegation::default(),
    }
}

fn default_version() -> u32 {
    1
}
fn default_true() -> bool {
    true
}
fn default_max_parallel() -> u32 {
    4
}
fn default_max_depth() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(pairs: &[(Effort, &str)]) -> Ramp {
        Ramp::Steps(pairs.iter().map(|(e, m)| (*e, m.to_string())).collect())
    }

    fn cfg(crew: BTreeMap<String, Ramp>) -> CrewConfig {
        CrewConfig {
            version: 1,
            planner: Planner {
                model: "opus".into(),
                may_delegate: true,
            },
            crew,
            delegation: Delegation::default(),
        }
    }

    #[test]
    fn ramp_picks_highest_floor_at_or_below() {
        let r = steps(&[(Effort::Tiny, "cheap"), (Effort::Large, "opus")]);
        assert_eq!(r.pick(Effort::Tiny), Some("cheap"));
        assert_eq!(r.pick(Effort::Small), Some("cheap"));
        assert_eq!(r.pick(Effort::Medium), Some("cheap"));
        assert_eq!(r.pick(Effort::Large), Some("opus"));
        assert_eq!(r.pick(Effort::Deep), Some("opus"));
    }

    #[test]
    fn ramp_below_lowest_floor_uses_lowest() {
        // floors start at small; a tiny request falls to the lowest floor.
        let r = steps(&[(Effort::Small, "sonnet"), (Effort::Large, "opus")]);
        assert_eq!(r.pick(Effort::Tiny), Some("sonnet"));
        assert_eq!(r.pick(Effort::Small), Some("sonnet"));
        assert_eq!(r.pick(Effort::Large), Some("opus"));
    }

    #[test]
    fn scalar_ramp_covers_all_efforts() {
        let r = Ramp::Single("cheap".into());
        for e in Effort::all() {
            assert_eq!(r.pick(e), Some("cheap"));
        }
    }

    #[test]
    fn resolve_falls_back_category_then_general_then_planner() {
        let mut crew = BTreeMap::new();
        crew.insert(
            "tests".into(),
            steps(&[(Effort::Tiny, "cheap"), (Effort::Deep, "opus")]),
        );
        crew.insert(GENERAL.into(), Ramp::Single("sonnet".into()));
        let c = cfg(crew);

        // category hit
        let r = c.resolve("tests", Effort::Small);
        assert_eq!(r.model, "cheap");
        assert_eq!(r.via, ResolveVia::Category);
        assert!(!r.fell_back);

        // unknown category -> general
        let r = c.resolve("frontend", Effort::Medium);
        assert_eq!(r.model, "sonnet");
        assert_eq!(r.via, ResolveVia::General);
        assert!(r.fell_back);

        // no general -> planner
        let mut crew2 = BTreeMap::new();
        crew2.insert("tests".into(), Ramp::Single("cheap".into()));
        let c2 = cfg(crew2);
        let r = c2.resolve("frontend", Effort::Medium);
        assert_eq!(r.model, "opus");
        assert_eq!(r.via, ResolveVia::Planner);
        assert!(r.fell_back);
    }

    #[test]
    fn yaml_accepts_scalar_and_map_shapes() {
        let yaml = "\
version: 1
planner:
  model: opus
crew:
  docs: cheap
  tests:
    tiny: cheap
    large: opus
";
        let c: CrewConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(c.resolve("docs", Effort::Deep).model, "cheap");
        assert_eq!(c.resolve("tests", Effort::Tiny).model, "cheap");
        assert_eq!(c.resolve("tests", Effort::Large).model, "opus");
        assert_eq!(c.delegation.max_parallel, 4); // default applied
    }

    #[test]
    fn validate_catches_unknown_models_and_missing_general() {
        let mut available = BTreeSet::new();
        available.insert("opus".to_string());
        available.insert("cheap".to_string());

        // missing `general`
        let mut crew = BTreeMap::new();
        crew.insert("tests".into(), Ramp::Single("cheap".into()));
        assert!(cfg(crew).validate(&available).is_err());

        // unknown model
        let mut crew = BTreeMap::new();
        crew.insert(GENERAL.into(), Ramp::Single("ghost".into()));
        assert!(cfg(crew).validate(&available).is_err());

        // valid
        let mut crew = BTreeMap::new();
        crew.insert(GENERAL.into(), Ramp::Single("cheap".into()));
        crew.insert(
            "tests".into(),
            steps(&[(Effort::Tiny, "cheap"), (Effort::Deep, "opus")]),
        );
        assert!(cfg(crew).validate(&available).is_ok());
    }

    #[test]
    fn default_with_tiers_is_valid_and_expands() {
        let c = default_with_tiers("cheap", "sonnet", "opus");
        let available: BTreeSet<String> = ["cheap", "sonnet", "opus"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        c.validate(&available).unwrap();
        // exploration ramps from cheap up to premium at deep.
        assert_eq!(c.resolve("exploration", Effort::Tiny).model, "cheap");
        assert_eq!(c.resolve("exploration", Effort::Deep).model, "opus");
        // every category expands to 5 concrete levels.
        assert_eq!(c.expanded("backend").len(), 5);
    }
}
