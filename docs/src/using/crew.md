# The crew (model routing)

Cowboy can route delegated work to **different models by the kind of work**. The
**foreman** is simply the model you select with `/model` (or `cowboy models use`):
it plans, delegates, reviews, and integrates. When it delegates a sub-task, it
describes the work by *category* and *effort*; Cowboy resolves that to a model
from your **crew roster**.

> The foreman requests a *kind* of worker — never a model. Model assignment is
> your routing policy. Quotas, rate limits, and spend belong to the
> [LLM gateway](../security/network.md), not to Cowboy.

## Solo or crew

Crew is opt-in. The `/model` picker has a **Solo / Crew** toggle (Tab):

- **Solo** — the selected model does everything itself; no delegation.
- **Crew** — the selected model is the foreman and delegates per your roster.

There's no separate "planner" setting: the foreman is always whatever `/model`
points at, so the picker can never disagree with what's actually running.
Switching the foreman is just selecting a different model.

## The roster — `~/.config/cowboy/crew.yaml`

Host-owned, like `models.yaml` (the agent can't read or edit it). Each category
is an **effort ramp**: a model is assigned a *floor* effort and handles that level
and everything above it, until a higher floor takes over. So a category can be one
model for all efforts, a couple of breakpoints, or all five spelled out.

There is no `planner:` field — the foreman is the selected `/model`. A roster
slot can name the special value `<default>` to mean "use the foreman," so you can
pin some efforts to specific models and inherit the foreman for the rest.
Swapping `/model` then re-points every `<default>` slot automatically.

```yaml
version: 1
crew:
  docs: cheap            # one model for every effort
  tests:
    tiny: cheap          # tiny..medium → cheap
    large: opus          # large, deep  → opus
  e2e:
    small: cheap         # ≤ medium → cheap
    large: <default>     # large, deep → the foreman (whatever /model selects)
  general: sonnet        # required: the cross-category fallback
temperature:             # optional: override temperature per task type
  tests: 0.0             #   cooler for precise work…
  exploration: 0.6       #   …warmer for ideation (falls back to general's, else
                         #   the model's own default)
delegation:
  enabled: true          # crew mode on (toggle Solo/Crew from the /model picker)
  max_parallel: 4        # local fan-out hint (not a quota)
  max_depth: 1           # foreman delegates; workers don't, by default
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
3. no match → the foreman.

A `<default>` slot at any step resolves to the foreman. So routing is total —
every request gets a model, worst case the foreman.

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

Categories: `general exploration backend frontend tests docs debugging
refactor e2e` (unknown → `general`). Effort defaults to `medium`. Each routed
launch is recorded as a `SubagentRouted` lifecycle event.

`effort` now sizes the worker's **turn grant** as well as its model (see [Turn
grants](#turn-grants-report-progress-request-more)), so size each delegation to fit
one worker: "review these two crates" will run out mid-way, whereas one job per
crate fans out and finishes. A worker already at the delegation depth limit is not
offered the `subagent` tool at all, so it cannot waste a round trip discovering
that it cannot delegate.

## Parallel delegation

Delegation is **asynchronous**. `subagent` returns a job id immediately and the
worker runs in the background, so the foreman keeps working: it can investigate
something else, delegate more, or answer you while children run. Each result is
delivered into its context as a message when that job finishes — it never has to
poll. Three tools support this:

| Tool | What it does |
|---|---|
| `jobs` | list the running jobs with their turn usage |
| `wait` | park until something lands (bounded, and interruptible) |
| `job_reply` | answer a blocked worker — a question, or a request for more turns |

The foreman cannot call `final` while jobs are still running: their results are
part of the task. It is refused twice, after which Cowboy waits on its behalf
rather than arguing indefinitely.

Fan-out is capped by `delegation.max_parallel` (a local throughput hint; the
gateway is the real backpressure) and by `delegation.max_parallel_per_provider`,
which keeps a burst of same-model workers off one provider's rate limit. Both
caps are **session-wide**, so they still hold when dispatches span several turns.
Independent read/explore/review work parallelizes safely in the shared sandbox;
isolated parallel *writers* compose with [Ranch](../ranch/overview.md) worktrees.

Once the roster is set up, delegation is frictionless — no per-task approvals, no
budget gates. Configure the crew once, then let it work.

## Turn grants: report progress, request more

A worker does not get an open-ended iteration cap. It starts with a small grant
scaled to its `effort`, and if the task turns out to be bigger it must **report
its progress and ask for more turns**. This replaced a flat 100-iteration cap that
let a review subagent spend everything re-reading files and return a `[partial]`
with no report.

```yaml
delegation:
  iterations:            # initial grant per effort (sparse floors fill upward)
    tiny: 15
    small: 25
    medium: 40
    large: 60
    deep: 80
  max_total_iterations: 400   # hard per-job ceiling; 0 disables supervision
  request_timeout_seconds: 120
  stall_window: 8             # iterations with nothing new → an early report
```

A request happens two ways: **voluntarily**, when the worker realises the task is
larger than its grant (the `request_turns` tool), and **compulsorily**, when the
grant runs out or the host notices it has stalled. Either way the worker pauses and
the foreman receives its report — plus **measured evidence** Cowboy attaches
itself: files read, edits made, commands run, and whether the last several steps
produced anything new. That way an extension is judged against what happened, not
against the worker's own optimism.

The foreman answers with `job_reply`:

| Verdict | Effect |
|---|---|
| `grant` | more turns, clamped host-side to the job's ceiling |
| `redirect` | more turns plus instructions to do something different |
| `wrap_up` | stop investigating; a few turns to write up what it has |
| `stop` | abandon the work; it still reports what it established |

Two bounds are the host's, not the model's. `max_total_iterations` caps the total
however many turns the foreman grants, and an **unanswered** request takes one
small automatic extension and then wraps up — so an unattended foreman can neither
leave a worker running forever nor destroy its work by ignoring it. A worker told
to wrap up always keeps enough turns to write its answer.

## A worker can ask a question

Turns are not the only thing a worker can be blocked on. When it hits a genuine
ambiguity — "the task says migrate the endpoints; does that include v1?" — it asks
over the same channel, and the foreman answers with
`job_reply` / `verdict: answer`:

```
[subagent api/medium · job 178…-sub1] is blocked on a question…

  migrate the v1 endpoints too?
  It suggested: yes · no
```

The foreman is the right respondent because it holds the context the worker lacks:
it wrote the task, and it can see the other workstreams. This replaced returning the
empty string, which the worker read as "proceed" — so it guessed, and the guess came
back as a confidently wrong result with no sign that there had been a fork in the
road.

Fail-open, deliberately: an unsupervised worker, or one whose question goes
unanswered, proceeds on its own judgement after a timeout rather than blocking
forever. That is the old behaviour, now the fallback instead of the rule. The
question and the answer are both journaled, so watching a worker (**Alt-w**) shows
why it paused rather than an unexplained gap.

## Stopping and steering

While a foreman is working you can:

- **type** — a message goes into the *running* turn, delivered at its next step,
  rather than waiting for the turn to finish;
- **`/after <msg>`** — queue a message to run as its own turn afterwards
  (`/queue` lists them, `/queue clear` drops them);
- **Alt-s** — stop the background subagents and leave the turn running;
- **Ctrl-C** — stop the turn and leave the subagents running. Their
  results arrive in a later turn, so correcting the foreman does not throw away
  minutes of delegated work.

Ending the session reaps every running worker.

## Partial results

A subagent that does real work but ends without a clean final answer — e.g. it
truncates a large output or errors out — doesn't return empty. Cowboy salvages a
checkpoint and hands it back prefixed `[partial]`: the
agent's latest narration, its plan progress, and the session id (whose
`.cowboy/sessions/<id>/` directory holds the full transcript, scratchpad, and
commands). The foreman is told to **resume from that checkpoint** — re-delegating
with the prior work as `context` — rather than restarting the task from scratch.
Subagents are also steered to stream large outputs to a file and publish them by
path, so a single oversized tool call can't lose a turn's work to truncation.

A subagent that fails outright — rather than ending with partial work — reports
*why* instead of a vague empty result: the foreman gets a `subagent error:` with
the cause (e.g. the host ran out of memory running too many at once, or the
[gateway](../security/network.md) returned a rate-limit/quota error), so the right
lever (lower `delegation.max_parallel`, or a gateway limit) is obvious.
