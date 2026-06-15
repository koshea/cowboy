# Crew Roster — build plan

Status: design, ready to build. Owner: koshea.

A powerful **planner** (foreman) coordinates a user-configured **crew** of
specialist models. The planner delegates work by *kind* (category + effort);
Cowboy resolves that to a model from the roster and runs the worker. Once the
roster is set up, delegation is **frictionless and parallel** — no approvals, no
budget gates.

> A powerful planner coordinates a configurable crew of specialist models, while
> Cowboy tracks routing and outcomes. Spend, quotas, and rate limits belong to the
> LLM gateway, not Cowboy.

---

## 1. Principles & invariants (load-bearing — do not violate)

1. **The planner requests a capability, never a model.** The `subagent` tool has
   no `model` field. The planner emits `category` + `effort`; Cowboy owns the
   mapping. This is the whole product thesis.
2. **The user owns routing; the gateway owns the rest.** Cowboy routes *work →
   logical model*. Quotas, rate limits, spend caps, retries, and backend
   failover are the **LLM gateway's** job (LiteLLM/etc.). Cowboy reimplements
   **none** of them. A roster model name may be a gateway routing alias.
3. **Efficiency over governance.** Configured = free to get to work. No
   per-delegation approval modal and no budget gates on the critical path.
   Telemetry is read-only.
4. **`crew.yaml` is host-owned.** Lives in `~/.config/cowboy/`, same trust class
   as `providers.yaml`/`models.yaml`: never mounted or readable inside the
   container. The agent cannot edit its own routing policy.
5. **The resolver is a shared core module.** Build it pure and standalone so a
   Ranch workstream can later declare a category/effort and route through the same
   roster. Crew (intra-session delegation) and Ranch (inter-session workstreams)
   converge on one router.

---

## 2. Concepts

- **Planner / foreman** — the main session model. Plans, decomposes, delegates,
  reviews, integrates. Configured separately from the crew.
- **Category** — the *kind* of work: `general exploration backend frontend tests
  review docs debugging refactor e2e` (open set; unknown → `general`).
- **Effort** — complexity on an ordered scale: `tiny < small < medium < large <
  deep` (unknown → `medium`).
- **Effort ramp** — per category, models assigned a *floor* effort; a model covers
  its floor and everything above until a higher floor takes over. Sparse by
  default, fully specifiable when wanted.
- **Resolver** — pure function `(category, effort) -> model`.
- **Worker / subagent** — a delegated run for a scoped task, executed with the
  resolved model.

---

## 3. Config — `~/.config/cowboy/crew.yaml`

Three shapes per category, smallest-effort-first:

```yaml
version: 1

planner:
  model: opus          # the main session model's default; per-session override allowed

crew:
  # scalar — one model for every effort (the common case)
  docs: cheap

  # ramp — sparse floors; each model covers from its floor upward
  tests:
    tiny: cheap        # tiny..medium → cheap
    large: opus        # large, deep  → opus

  backend:
    small: sonnet      # tiny..medium → sonnet (tiny falls to the lowest floor)
    large: opus        # large, deep  → opus

  # fully explicit (all five) — for non-monotonic cases
  review:
    tiny: sonnet
    small: sonnet
    medium: opus
    large: opus
    deep: opus

  general: sonnet      # required: the cross-category fallback

delegation:
  max_parallel: 4      # local throughput HINT (avoid local churn); NOT a quota
  max_depth: 1         # planner delegates; workers do not, by default
  allow_recursive_delegation: false
```

Model names (`cheap`/`sonnet`/`opus`) are entries from the existing model
catalogue (`models.yaml`, user+project merge), which resolve through the gateway.

### Resolution rule

Effort scale: `tiny(0) < small(1) < medium(2) < large(3) < deep(4)`.

Within a category, given requested effort **E**:
1. Sort the defined `(floor, model)` entries by effort.
2. Pick the model at the **highest floor ≤ E**.
3. If **E** is below every floor, use the **lowest** floor's model. *(Confirmed:
   undefined-bottom falls to the lowest floor, not to `general`.)*

Cross-layer fallback (when a category itself is undefined or empty):
```
category(effort) → general(effort) → planner.model
```
Scalar `category: model` is sugar for a single floor at `tiny`.

### Validation (`crew validate`)

- every referenced model exists in the resolved catalogue;
- `planner.model` exists;
- each defined category has ≥ 1 entry; `general` is defined;
- effort keys are valid level names;
- `delegation` fields are sane (`max_parallel ≥ 1`, `max_depth ≥ 0`).
No gap-checking is needed — the resolution rule is total over any non-empty ramp.

---

## 4. The resolver — `cowboy-core/src/crew.rs` (Phase 0)

Pure, no I/O beyond load/save. Mirrors the shape of `ranch.rs`/`scope.rs`.

```rust
pub enum Effort { Tiny, Small, Medium, Large, Deep }   // Ord by rank
pub struct CrewConfig {
    pub version: u32,
    pub planner: Planner,                 // { model, .. }
    pub crew: BTreeMap<String, Ramp>,     // category -> ramp
    pub delegation: Delegation,           // max_parallel, max_depth, allow_recursive
}
pub struct Resolved { pub model: String, pub fell_back: bool, pub via: ResolveVia }

impl CrewConfig {
    pub fn resolve(&self, category: &str, effort: Effort) -> Resolved; // the rule above
    pub fn validate(&self, catalogue: &ModelCatalogue) -> Result<()>;
}
pub fn path() -> PathBuf;                 // ~/.config/cowboy/crew.yaml
pub fn load() -> Result<CrewConfig>;
pub fn save(&CrewConfig) -> Result<()>;
pub fn default_from_catalogue(&ModelCatalogue) -> CrewConfig; // tiers from price
```

- `Ramp` deserializes from scalar **or** map (serde untagged / custom) → both
  config shapes above.
- `default_from_catalogue` derives ~3 price tiers (cheapest / mid / strongest)
  and emits ramps (`cheap` floor + `opus` floor at `large`) so `crew init` is
  good out of the box.
- Unit-test the resolver exhaustively: scalar, sparse, full, below-lowest-floor,
  unknown category → general, unknown effort → medium, general-missing → planner.

---

## 5. Phase 1 — Routing on the (sequential) subagent path

Goal: planner delegates by category/effort; Cowboy routes to the configured
model; user can see and edit the roster. **No new parallelism yet.**

1. **Core + CLI:** `crew.rs` (Phase 0) + `cowboy crew init | list | show |
   validate` (`cmd/crew.rs`, wired in `cli.rs`/`main.rs`/`cmd/mod.rs`). `list`
   prints the resolved 5-wide matrix (ramps expanded) so users see effective
   routing.
2. **Subagent tool schema** (`agent/tools.rs::SubagentArgs`): add `category`,
   `effort`, `reason`, `expected_artifact`. Defaults `general`/`medium`; unknown
   values resolve-and-log, never error. Updates the tool-surface snapshot +
   `definitions_cover_the_tool_surface`.
3. **`run_subagent`** (`agent/run.rs`): load crew, `resolve(category, effort)`,
   pass the resolved model into the nested `cowboy` worker (today it inherits the
   session model — thread `--model <resolved>` / env instead). Enforce
   `max_depth`/no-recursive via the existing `COWBOY_SUBAGENT_DEPTH` guard tied to
   config.
4. **Planner prompt:** foreman guidance — when to delegate (scoped/separable,
   exploration, test authoring, review, artifact-producing subtasks) and when not
   (tiny, hand-off cost > benefit, needs continuous coordination); "request a
   *kind*, not a model"; give a concrete **effort rubric** with examples (models
   self-rate effort poorly).
5. **Logging:** new `LifecycleEvent::SubagentRouted { category, effort, model,
   fell_back, reason, expected_artifact }` → existing `lifecycle.jsonl`.
6. **TUI read-only crew sidebar:** planner + active/recent delegations
   (category/effort, model, status, running cost — cost already tracked in
   `AgentLoop`). Reuse the existing overlay/panel patterns in `cowboy-tui`.
7. **TUI roster editor + model picker:** reuse the `ModelPicker` overlay + the
   `SwitchModel` plumbing. Ramp-aware grid (see §7). `/crew`, key `c`.

**Acceptance (Phase 1):** crew.yaml validates; CLI works; the subagent tool takes
category/effort; subagents run with the resolved model; routing is logged; the
TUI shows crew activity and can edit the roster + planner; recursive delegation
blocked.

---

## 6. Phase 2 — Parallel dispatch (the efficiency payoff)

The headline. "Free to get to work" = the crew runs concurrently.

1. **Concurrency from batched tool calls.** When the planner emits *multiple*
   `subagent` calls in one assistant turn, `handle_tool_calls` runs them
   **concurrently and joins**, instead of the current sequential loop. No new tool
   or syntax — the model already batches tool calls. Non-subagent calls in the
   same batch keep today's behavior.
2. **Readers vs writers:**
   - **Parallel readers** (exploration / review / analysis / reading) share the
     one container safely → ship first; low risk, immediate speedup.
   - **Parallel writers** to `/workspace` race → route them through the **Ranch
     worktree machinery we already built** (one writable session per worktree +
     lease). This is where Crew and Ranch converge. Add after readers.
3. **`max_parallel` = throughput hint**, capping local fan-out (container/IO
   churn), **not** a quota — the gateway is the real backpressure; the `model.rs`
   429 backoff is the safety net.
4. **Throughput sidebar:** workers running vs queued, wall-clock vs serial
   estimate, what's blocking (waiting on gateway vs slow worker). Measures
   *throughput*, not dollars.
5. **Handoffs back to the planner:** subagents already emit `handoff.md` (the
   `handoff` tool + auto-generated). Return a **concise summary** (status, changed
   files, risks) + the handoff path — not the raw transcript — so the planner's
   context stays lean (latency win on every subsequent turn). Store under
   `…/sessions/<parent>/subagents/<id>/`.
6. **Model-client reuse:** pool the provider HTTP client across workers rather
   than building a fresh TLS stack per worker.

**Acceptance (Phase 2):** the planner can fan out N delegations that run
concurrently and join; parallel readers share the container; parallel writers get
isolated worktrees; the sidebar shows real concurrency/throughput; handoffs return
concise summaries; no approval/budget gating anywhere.

---

## 7. TUI details

- **Sidebar (read-only):** `planner opus active`, then `tests/small cheap done`,
  `review/deep opus running`, etc. Fields: role or category/effort, model, status,
  running cost, (Phase 2) concurrency/throughput.
- **Roster editor (ramp-aware):** five effort columns per category, but you set
  **breakpoints (floors)** only. Inherited cells render dim showing the model they
  *resolve* to; editing a cell adds a floor; clearing removes it and the span
  re-flows. Set planner model. Validate / save / discard.
- **Model picker:** reuse the existing overlay; lists catalogue models (names are
  gateway-resolvable). `[enter] select` `[esc] cancel`. (No "test/inspect" needed
  for v1.)
- **Commands / keys:** `/crew`, `/crew edit`, `/crew validate`; key `c` opens the
  panel. In the grid: arrows move · `enter` edit cell · `p` planner model · `v`
  validate · `s` save · `r` clear cell · `esc` close.

---

## 8. Phase 3 — Adaptive tuning (recommend-only; gated on real signals)

Reframed away from spend (the gateway has that) toward **latency / parallelism /
rework**.

- **Define "success" operationally first** (capture these in Phase 2 logging):
  expected_artifact produced? planner reworked the output? tests passed after?
  This is the prerequisite — recommendations on vibes are worthless.
- **Outcome store** (home-level, append-only) keyed by category/effort/model.
- `cowboy crew recommend` + a TUI modal: evidence-based suggestions ("cheap
  succeeded on 18/20 recent tests/small at far lower latency — route tests/small's
  small-floor to cheap"). **Apply via diff; never auto-apply.**
- Policy modes (`cheap | balanced | quality | manual`) as hints to the
  recommender, not silent routers.
- Optional category-detection warning when a delegated category looks wrong.

**Acceptance (Phase 3):** outcomes recorded with trustworthy signals; `recommend`
produces evidence-backed diffs; nothing changes the roster without approval.

---

## 9. Convergence with Ranch

Once the resolver is a core module: a Ranch **workstream** declares a
`category`/`effort`, and the coordinator routes it through the **same roster** +
the **same worktree isolation** used by parallel writers above. Crew = ephemeral
intra-session delegation; Ranch = persistent inter-session workstreams; one
router, one mental model ("foreman + crew"). Design the resolver API for this now;
wire Ranch later. (Out of scope for Crew v1.)

---

## 10. Security

- `crew.yaml` host-owned in `~/.config/cowboy/`; never mounted/masked into the
  container — same as `models.yaml`. The agent cannot read or edit its routing.
- The "planner can't pick a model" rule is enforced structurally: the `subagent`
  tool has no `model` field.
- Parallel writers stay inside the existing security boundary (each is a normal
  Cowboy session in its own worktree, with the full container + gateway
  enforcement and the one-writable-lease guarantee).

---

## 11. Build order

Phase 0 → 1 give the walking skeleton; Phase 2 gives the payoff.

1. `crew.rs` config structs + `Ramp` (scalar|map) + `resolve` + `validate` + unit
   tests. **(P0)**
2. `cowboy crew init | list | show | validate`. **(P1)**
3. Planner model: read `planner.model` from crew as the session default. **(P1)**
4. `SubagentArgs` → category/effort/reason/expected_artifact (+ snapshot). **(P1)**
5. `run_subagent`: resolve + run the worker with the resolved model. **(P1)**
6. Depth/recursive guards from config. **(P1)**
7. `SubagentRouted` lifecycle logging. **(P1)**
8. TUI read-only crew sidebar. **(P1)**
9. TUI roster editor + model picker + save/validate. **(P1)**
10. Concurrent batched subagent calls (parallel readers). **(P2)**
11. Parallel writers via Ranch worktrees. **(P2)**
12. Throughput sidebar + concise handoff-back. **(P2)**
13. Outcome signals + `crew recommend` (diff, recommend-only). **(P3)**

**First PR / demo:** items 1–5 + 8 (resolver + routing + read-only sidebar). That
alone makes Cowboy feel like a model-orchestration cockpit.

---

## 12. Non-goals (for this feature)

Budget/quota/rate-limit enforcement in Cowboy (gateway owns it); approval/spend
gating on the critical path; provider-specific or non-OpenAI-compatible APIs;
opaque automatic model selection; unbounded recursive delegation; cloud/team
roster sync; model leaderboards/benchmarking; A2A; native Linear. Ranch dependency
orchestration is deferred but designed to converge (§9).

---

## 13. Open questions to confirm before/while building

- **Parallel writers in v1, or readers-only first?** Recommendation: ship
  read/explore/review parallelism first (low risk, immediate "it's fast"), add
  worktree-isolated writers next.
- **Project-level `crew.yaml` override** (`.cowboy/crew.yaml`, host-owned, never
  mounted) — user-global is enough for v1; add project override later if wanted.
- **Effort levels:** keep all five, or start with three? Five is fine given ramps
  rarely fill all slots.
- **Surfacing gateway usage** (token usage / rate-limit headers) read-only in the
  sidebar — nice-to-have, gateway-agnostic.
