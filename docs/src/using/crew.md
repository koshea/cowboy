# The crew (model routing)

Cowboy can route delegated work to **different models by the kind of work**. The
**foreman** is the model the session runs — the one you select with `/model` (or
`cowboy models use`), or the crew's own `foreman:`:
it plans, delegates, reviews, and integrates. When it delegates a sub-task, it
describes the work by *category* and *effort*; Cowboy resolves that to a model
from your **crew roster**.

> The foreman requests a *kind* of worker — never a model. Model assignment is
> your routing policy. Quotas, rate limits, and spend belong to the
> [LLM gateway](../security/network.md), not to Cowboy.

## Solo or crew

Crew is opt-in. In the `/model` picker, **Tab** cycles the mode for the model you
pick: **solo**, then each of your crews.

- **Solo** — the selected model does everything itself; no delegation.
- **A crew** — the selected model is the foreman and delegates per that crew's
  roster.

There's no separate "planner" setting: the foreman is whatever `/model` points at,
unless the crew names its own (`foreman:`, below).

## Several crews

You can keep several crews and switch between them: an open-source-heavy crew on
your local gateway, a high-cost crew for hard problems, an all-OpenAI crew on your
[ChatGPT subscription](chatgpt.md). They live side by side in `crew.yaml`, and
**`active`** names the one new sessions start with:

```sh
cowboy crew use premium   # new sessions start with `premium` (and crew mode is on)
cowboy crew solo          # crew mode off: the foreman works alone
```

```text
/crew                     # this session's crew, the others, and its roster
/crew premium             # switch this session (only) to `premium`
```

A switch takes effect at the next delegation. A session keeps the crew it started
with even if `active` changes later, and its subagents use their parent's crew.
Delegations already running finish on the crew that dispatched them.

A crew can name its **`foreman`**. Switching to that crew also switches the session's
model to the foreman, and the crew's `<default>` slots resolve to it. `/model` still
overrides it for the rest of the session. A crew without `foreman:` keeps whatever
`/model` selects. A switch made mid-turn changes the model when that turn finishes;
it does not cancel the turn.

## The roster — `~/.config/cowboy/crew.yaml`

Host-owned, like `models.yaml` (the agent can't read or edit it). Each category
is an **effort ramp**: a model is assigned a *floor* effort and handles that level
and everything above it, until a higher floor takes over. So a category can be one
model for all efforts, a couple of breakpoints, or all five spelled out.

There is no `planner:` field — the foreman is the session's model: the crew's
`foreman:` if it names one, else the selected `/model`. A roster slot can name the
special value `<default>` to mean "use the foreman," so you can pin some efforts to
specific models and inherit the foreman for the rest. Swapping `/model` then
re-points every `<default>` slot automatically.

Slots name models as `provider/name` (see
[models.yaml](../getting-started/configuration.md#modelsyaml--user-configcowboy-andor-project-cowboy)),
so one roster can route to several providers — a local gateway for the cheap,
high-volume work and a hosted model for the hard parts. A bare name in a slot is a
[harness](#harnesses) or `<default>`; anything else is refused with a
pointer at `cowboy models migrate`.

```yaml
version: 1
active: oss                      # the crew new sessions start with
delegation:                      # shared by every crew
  enabled: true                  # crew mode on (solo when off; Tab in the /model picker)
  max_parallel: 4                # local fan-out hint (not a quota)
  max_depth: 1                   # foreman delegates; workers don't, by default
  allow_recursive_delegation: false
crews:
  oss:
    crew:
      docs: bifrost/cheap        # one model for every effort
      tests:
        tiny: bifrost/cheap      # tiny..medium → bifrost/cheap
        large: bifrost/glm       # large, deep  → bifrost/glm
      e2e:
        small: bifrost/cheap
        large: <default>         # large, deep → the foreman
      general: bifrost/glm       # required: the cross-category fallback
    temperature:                 # optional: override temperature per task type
      tests: 0.0                 #   cooler for precise work…
      exploration: 0.6           #   …warmer for ideation
  openai:
    foreman: chatgpt/gpt-6.1-sol@medium # optional: switching here switches the model
                                 # (and, with `@level`, the reasoning level) too
    crew:
      exploration: chatgpt/gpt-6-luna
      general: <default>
    delegation:                  # optional: per-crew overrides of the shared limits
      max_parallel_per_provider: 1
```

Each crew has its own `crew`, `temperature` and `descriptions`. The `delegation`
block is shared; a crew may override any of its limits except `enabled`, which is
one switch for all crews.

**Upgrading.** A `crew.yaml` from before named crews (a top-level `crew:` roster)
is refused with a pointer at `cowboy models migrate`. The migration turns the
roster into the crew `default`, makes it active, and keeps its comments.

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

### Reasoning levels per slot

A slot or `foreman:` can name a reasoning level after the model, as `@level`:

```yaml
crews:
  openai:
    foreman: chatgpt/gpt-6.1-sol@medium
    crew:
      general:
        tiny: chatgpt/gpt-6-luna         # the model's own level
        medium: chatgpt/gpt-6.1-sol@medium
        large: chatgpt/gpt-6.1-sol@high
        deep: chatgpt/gpt-6-astra@xhigh
      tests:
        tiny: <default>@low              # the foreman's model, at `low`
```

The levels are `minimal`, `low`, `medium`, `high`, `xhigh` and `max` (`ultra` is read
as `max`; see [reasoning levels](../getting-started/configuration.md#modelsyaml--user-configcowboy-andor-project-cowboy)).
A subagent routed to a slot runs at its level; a slot without one runs at its model's
own `reasoning_effort`, whatever level its parent runs at. A session on a crew whose
foreman names a level starts at it, and switching to that crew sets it (`/reasoning`
changes it afterwards). A level on a [harness](#harnesses) slot is ignored, because the
harness runs its own model.

`cowboy crew validate` (and `doctor`) report a level the model's `reasoning_levels`
don't include, with the level it would run at instead. It still runs: the level is
stepped to the nearest one the model accepts.

## Managing it

```sh
cowboy crew init                  # write crew.yaml with a `default` crew (tiers from model prices)
cowboy crew new <name> [--from c] # add a crew: a copy of c, or price tiers
cowboy crew new openai --preset chatgpt   # the built-in all-OpenAI crew (see How-to)
cowboy crew copy <from> <to>      # duplicate a crew to tweak it
cowboy crew rename <from> <to>    # (the active pointer follows)
cowboy crew remove <name>         # not the active one
cowboy crew use <name> | solo     # what new sessions start with
cowboy crew list [--crew c]       # the crews, and a routing matrix (category × effort → model)
cowboy crew show                  # the full crew.yaml
cowboy crew validate              # every crew: models and foremen exist, providers configured
                                  #   and signed in, `general` defined
cowboy crew usage [--crew c]      # recorded activity per model (tasks, success %, avg time)
```

`crew init` and `crew new` rank your models by price into three tiers (cheap /
standard / premium) and emit sensible ramps — edit from there. These commands edit
`crew.yaml` in place where they can, so your comments survive. Inside an interactive
session, `/crew` shows the session's crew and `/crew usage` its activity.

## Usage tracking

Each routed delegation appends a small outcome record (crew, category, effort,
model, status, duration — never the task text) to `~/.config/cowboy/crew-history.jsonl`.
`--crew` on `usage` and `recommend` narrows to one crew. Records from before named
crews carry no crew and appear only in the unfiltered view.
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
refactor e2e review` (unknown → `general`). Effort defaults to `medium`. Each
routed launch is recorded as a `SubagentRouted` lifecycle event.

### What a category means

A category is a free-form string, so the foreman would otherwise have to infer
what `exploration` or `review` covers from the word alone — and its guess need not
match what you meant when you wrote the roster. So the meaning is stated, not
inferred: Cowboy ships a definition for each category above and injects it into the
foreman's system prompt alongside the name. Override any of them, or define your
own category, under `descriptions:` — your wording goes into the prompt verbatim,
so the sentence that documents the slot *is* the sentence the model routes on.

```yaml
crews:
  oss:
    crew:
      perf:
        medium: bifrost/cheap
        large: bifrost/premium
    descriptions:
      perf: "profiling and optimisation; must show a before/after measurement"
      review: "read the diff and report problems — never edit, never 'just fix it'"
```

`cowboy crew list` prints the meaning in force for every category, so you can see
exactly what the agent is routing on and spot a slot that is still guessing.

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
| `wrap_up` | stop investigating; six turns to write up what it has |
| `stop` | abandon the work; it still reports what it established |

Two bounds are the host's, not the model's. `max_total_iterations` caps the total
however many turns the foreman grants, and an **unanswered** request takes one
small automatic extension and then wraps up — so an unattended foreman can neither
leave a worker running forever nor destroy its work by ignoring it. A worker told
to wrap up always keeps enough turns to write its answer — six, because writing up
honestly costs a file write, an `artifact` publish, a `handoff`, and `final`, and an
earlier allowance of three ran out on the last of those.

**Wrapping up is enforced, not requested.** Once a worker is in wrap-up the host
narrows its tool surface to the reporting tools — `final`, `artifact`, `handoff`,
`decision`, `memory`, `blocked`/`unblock`, and the `jobs`/`wait`/`job_reply` it may
need to collect delegated work — and refuses anything else it calls anyway, since
its own history is full of earlier `shell` and `read` calls to reach for. "Stop
investigating" as advice does not work: a real worker answered it with fourteen more
`grep`s, hit its ceiling, was stopped, and lost seventy turns of investigation
because it had written none of it down. `jobs`/`wait` stay available deliberately —
`final` refuses while delegated work is in flight, so a foreman denied the means to
collect it would have no legal move left.

If a worker does run out before calling `final`, its result is a `[partial]`
checkpoint that **leads with what it actually produced** — the artifacts it published
and the handoff it wrote, as recorded by the host rather than claimed by the worker.
That matters because a worker generally runs out *before* ticking the last box on its
plan, so its own plan can say "write the findings" for a document it has already
written; the plan is still shown, marked as its own possibly-stale account.

Which is the case for writing things down *as you go*. A worker that narrates its
findings turn by turn survives being stopped; one that only accumulates tool results
has nothing to salvage, because the conclusions were never anywhere but the model's
head. Cowboy can force the report at the end, but it cannot reconstruct reasoning
that was never written.

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

## Harnesses

A category can route to another vendor's agent CLI instead of a model — e.g.
`exploration: grok` — to use your subscription with it. See
[Harnesses](harnesses.md).
