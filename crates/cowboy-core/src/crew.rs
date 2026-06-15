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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrewConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub planner: Planner,
    #[serde(default)]
    pub crew: BTreeMap<String, Ramp>,
    /// Optional per-category temperature override (by task type). A delegated
    /// task in this category runs at this temperature instead of the model's
    /// default — e.g. tests/refactor cooler, exploration warmer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub temperature: BTreeMap<String, f32>,
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

    /// The temperature override for a category (falling back to `general`'s, then
    /// none → the model keeps its own default).
    pub fn temperature_for(&self, category: &str) -> Option<f32> {
        self.temperature
            .get(category)
            .or_else(|| self.temperature.get(GENERAL))
            .copied()
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
        temperature: BTreeMap::new(),
        delegation: Delegation::default(),
    }
}

// ---------------------------------------------------------------------------
// Outcome history  (~/.config/cowboy/crew-history.jsonl — host-owned)
// ---------------------------------------------------------------------------

/// One recorded delegation outcome (appended after each routed subagent runs).
/// Carries no task text — just the routing + a coarse result — so it's safe to
/// aggregate across projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewOutcome {
    pub ts_ms: u64,
    pub category: String,
    pub effort: String,
    pub model: String,
    pub fell_back: bool,
    /// "complete" | "empty" | "error".
    pub status: String,
    pub duration_ms: u64,
}

impl CrewOutcome {
    pub fn succeeded(&self) -> bool {
        self.status == "complete"
    }
}

/// The home outcome log (`~/.config/cowboy/crew-history.jsonl`).
pub fn history_path() -> Option<PathBuf> {
    crate::config::global_config_dir().map(|d| d.join("crew-history.jsonl"))
}

/// Append one outcome (best-effort; creates the dir). Errors are swallowed so a
/// telemetry write never breaks a session.
pub fn record_outcome(outcome: &CrewOutcome) {
    let Some(p) = history_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(outcome) else {
        return;
    };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Load all recorded outcomes (skips malformed lines).
pub fn load_history() -> Vec<CrewOutcome> {
    let Some(p) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<CrewOutcome>(l).ok())
        .collect()
}

/// Per-model usage summary aggregated from outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub model: String,
    pub tasks: usize,
    pub succeeded: usize,
    pub total_duration_ms: u64,
}

impl UsageRow {
    pub fn avg_duration_ms(&self) -> u64 {
        self.total_duration_ms
            .checked_div(self.tasks as u64)
            .unwrap_or(0)
    }
    pub fn success_pct(&self) -> u32 {
        (self.succeeded * 100).checked_div(self.tasks).unwrap_or(0) as u32
    }
}

/// Aggregate outcomes by model (sorted by task count, descending).
pub fn usage_by_model(outcomes: &[CrewOutcome]) -> Vec<UsageRow> {
    let mut by: BTreeMap<String, UsageRow> = BTreeMap::new();
    for o in outcomes {
        let row = by.entry(o.model.clone()).or_insert_with(|| UsageRow {
            model: o.model.clone(),
            tasks: 0,
            succeeded: 0,
            total_duration_ms: 0,
        });
        row.tasks += 1;
        if o.succeeded() {
            row.succeeded += 1;
        }
        row.total_duration_ms += o.duration_ms;
    }
    let mut rows: Vec<_> = by.into_values().collect();
    rows.sort_by(|a, b| b.tasks.cmp(&a.tasks).then(a.model.cmp(&b.model)));
    rows
}

/// Aggregate by exact route (category, effort, model) — the grain recommendations
/// reason over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStat {
    pub category: String,
    pub effort: String,
    pub model: String,
    pub tasks: usize,
    pub succeeded: usize,
    pub fell_back: usize,
}

impl RouteStat {
    pub fn success_pct(&self) -> u32 {
        (self.succeeded * 100).checked_div(self.tasks).unwrap_or(0) as u32
    }
}

/// Aggregate outcomes by (category, effort, model).
pub fn stats_by_route(outcomes: &[CrewOutcome]) -> Vec<RouteStat> {
    let mut by: BTreeMap<(String, String, String), RouteStat> = BTreeMap::new();
    for o in outcomes {
        let key = (o.category.clone(), o.effort.clone(), o.model.clone());
        let row = by.entry(key).or_insert_with(|| RouteStat {
            category: o.category.clone(),
            effort: o.effort.clone(),
            model: o.model.clone(),
            tasks: 0,
            succeeded: 0,
            fell_back: 0,
        });
        row.tasks += 1;
        if o.succeeded() {
            row.succeeded += 1;
        }
        if o.fell_back {
            row.fell_back += 1;
        }
    }
    by.into_values().collect()
}

/// Minimum samples before we'll say anything about a route (avoid noise).
const RECOMMEND_MIN_SAMPLES: usize = 3;
/// Success rate below which a route is flagged for review.
const RECOMMEND_LOW_SUCCESS_PCT: u32 = 60;

/// Evidence-based, recommend-only suggestions from recorded outcomes. Returns
/// human-readable lines; NEVER mutates the roster (the user decides + edits).
pub fn recommend(outcomes: &[CrewOutcome]) -> Vec<String> {
    let mut out = Vec::new();
    let stats = stats_by_route(outcomes);
    for s in &stats {
        if s.tasks < RECOMMEND_MIN_SAMPLES {
            continue;
        }
        if s.success_pct() < RECOMMEND_LOW_SUCCESS_PCT {
            out.push(format!(
                "{}/{} via `{}`: only {}% success over {} tasks — consider a stronger model for this route.",
                s.category, s.effort, s.model, s.success_pct(), s.tasks
            ));
        }
        // Frequently falling back means the category/effort isn't in the roster.
        if s.fell_back >= RECOMMEND_MIN_SAMPLES && s.fell_back * 2 >= s.tasks {
            out.push(format!(
                "{}/{} fell back to `{}` {}/{} times — add `{}` to your roster to route it explicitly.",
                s.category, s.effort, s.model, s.fell_back, s.tasks, s.category
            ));
        }
    }
    out
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
            temperature: BTreeMap::new(),
            delegation: Delegation::default(),
        }
    }

    #[test]
    fn temperature_override_falls_back_to_general() {
        let mut c = cfg(BTreeMap::from([(
            GENERAL.to_string(),
            Ramp::Single("m".into()),
        )]));
        c.temperature.insert("tests".into(), 0.0);
        c.temperature.insert(GENERAL.into(), 0.5);
        assert_eq!(c.temperature_for("tests"), Some(0.0)); // explicit
        assert_eq!(c.temperature_for("frontend"), Some(0.5)); // general fallback
        let c2 = cfg(BTreeMap::from([(
            GENERAL.to_string(),
            Ramp::Single("m".into()),
        )]));
        assert_eq!(c2.temperature_for("tests"), None); // no overrides → model default
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
    fn usage_aggregates_by_model() {
        let o = |model: &str, status: &str, dur: u64| CrewOutcome {
            ts_ms: 1,
            category: "tests".into(),
            effort: "small".into(),
            model: model.into(),
            fell_back: false,
            status: status.into(),
            duration_ms: dur,
        };
        let outcomes = vec![
            o("cheap", "complete", 100),
            o("cheap", "complete", 300),
            o("cheap", "error", 50),
            o("opus", "complete", 1000),
        ];
        let rows = usage_by_model(&outcomes);
        assert_eq!(rows[0].model, "cheap"); // most tasks first
        assert_eq!(rows[0].tasks, 3);
        assert_eq!(rows[0].succeeded, 2);
        assert_eq!(rows[0].success_pct(), 66);
        assert_eq!(rows[0].avg_duration_ms(), 150);
        assert_eq!(rows[1].model, "opus");
        assert_eq!(rows[1].success_pct(), 100);
    }

    #[test]
    fn recommend_flags_low_success_and_frequent_fallback() {
        let o = |cat: &str, model: &str, status: &str, fb: bool| CrewOutcome {
            ts_ms: 1,
            category: cat.into(),
            effort: "small".into(),
            model: model.into(),
            fell_back: fb,
            status: status.into(),
            duration_ms: 10,
        };
        // tests/small via cheap: 1/4 success → flagged.
        let mut outcomes = vec![
            o("tests", "cheap", "complete", false),
            o("tests", "cheap", "error", false),
            o("tests", "cheap", "error", false),
            o("tests", "cheap", "empty", false),
        ];
        // frontend/small fell back to general's model 3/3 → flagged to add to roster.
        outcomes.extend([
            o("frontend", "sonnet", "complete", true),
            o("frontend", "sonnet", "complete", true),
            o("frontend", "sonnet", "complete", true),
        ]);
        let recs = recommend(&outcomes);
        assert!(recs
            .iter()
            .any(|r| r.contains("tests/small") && r.contains("success")));
        assert!(recs
            .iter()
            .any(|r| r.contains("frontend/small") && r.contains("fell back")));

        // Below the sample floor → silent.
        let few = vec![o("docs", "cheap", "error", false)];
        assert!(recommend(&few).is_empty());
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
