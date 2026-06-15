# The crew (model routing)

Cowboy can route delegated work to **different models by the kind of work**. The
main session model is the **foreman**: it plans, delegates, reviews, and
integrates. When it delegates a sub-task, it describes the work by *category* and
*effort*; Cowboy resolves that to a model from your **crew roster**.

> The planner requests a *kind* of worker — never a model. Model assignment is
> your routing policy. Quotas, rate limits, and spend belong to the
> [LLM gateway](../security/network.md), not to Cowboy.

## The roster — `~/.config/cowboy/crew.yaml`

Host-owned, like `models.yaml` (the agent can't read or edit it). Each category
is an **effort ramp**: a model is assigned a *floor* effort and handles that level
and everything above it, until a higher floor takes over. So a category can be one
model for all efforts, a couple of breakpoints, or all five spelled out.

```yaml
version: 1
planner:
  model: opus            # the foreman / main session model
crew:
  docs: cheap            # one model for every effort
  tests:
    tiny: cheap          # tiny..medium → cheap
    large: opus          # large, deep  → opus
  backend:
    small: sonnet        # ≤ medium → sonnet (tiny falls to the lowest floor)
    large: opus          # large, deep → opus
  general: sonnet        # required: the cross-category fallback
temperature:             # optional: override temperature per task type
  tests: 0.0             #   cooler for precise work…
  exploration: 0.6       #   …warmer for ideation (falls back to general's, else
                         #   the model's own default)
delegation:
  max_parallel: 4        # local fan-out hint (not a quota)
  max_depth: 1           # planner delegates; workers don't, by default
  allow_recursive_delegation: false
```

The optional `temperature` map overrides the sampling temperature **per category**
(task type): a delegated `tests` task runs cooler, an `exploration` task warmer,
regardless of the chosen model's default. Unlisted categories fall back to
`general`'s temperature, then to the model's own.

Model names are entries from your [model catalogue](../getting-started/configuration.md)
(`models.yaml`), so they resolve through the gateway — and a name can even be a
gateway routing alias.

### How a request resolves

Effort scale: `tiny < small < medium < large < deep`. For a `(category, effort)`
request:

1. the category's ramp picks the **highest floor ≤ effort** (or, below all
   floors, the **lowest** floor);
2. unknown category → the `general` ramp;
3. no match → the planner model.

So routing is total — every request gets a model, worst case the planner's.

## Managing it

```sh
cowboy crew init        # write a default roster (tiers derived from model prices)
cowboy crew list        # the routing matrix (category × effort → model)
cowboy crew show        # the full crew.yaml
cowboy crew validate    # check models exist, `general` defined, etc.
cowboy crew usage       # recorded activity per model (tasks, success %, avg time)
```

`crew init` ranks your models by price into three tiers (cheap / standard /
premium) and emits sensible ramps — edit from there. Inside an interactive
session, `/crew` shows the roster and `/crew usage` its activity.

## Usage tracking

Each routed delegation appends a small outcome record (category, effort, model,
status, duration — never the task text) to `~/.config/cowboy/crew-history.jsonl`.
`cowboy crew usage` aggregates it per model so you can see what your crew is
actually doing and how each model performs. There is no spend tracking here — the
[gateway](../security/network.md) owns cost and quotas.

## Delegating

The foreman delegates with the `subagent` tool, describing the work — not the
model:

```json
{
  "task": "Add regression tests for token refresh.",
  "category": "tests",
  "effort": "small",
  "reason": "isolated test-writing work",
  "expected_artifact": "changed test files + a short summary"
}
```

Categories: `general exploration backend frontend tests review docs debugging
refactor e2e` (unknown → `general`). Effort defaults to `medium`. Each routed
launch is recorded as a `SubagentRouted` lifecycle event.

## Parallel delegation

When the foreman delegates **several** sub-tasks in one turn, Cowboy runs them
**concurrently** and joins the results — the efficiency payoff. Fan-out is capped
by `delegation.max_parallel` (a local throughput hint; the gateway is the real
backpressure). Independent read/explore/review work parallelizes safely in the
shared container; isolated parallel *writers* compose with
[Ranch](../ranch/overview.md) worktrees.

Once the roster is set up, delegation is frictionless — no per-task approvals, no
budget gates. Configure the crew once, then let it work.
