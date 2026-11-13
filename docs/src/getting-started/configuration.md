# Configuration

Three files live under `.cowboy/`. `cowboy init` writes commented defaults. A
deeper field reference is in [Config files](../reference/config-files.md).

## `security.yaml` (host-owned, never visible to the agent)

Read only by the host `cowboy` process. Controls what the sandbox can see, its
resource ceilings, the network policy, and secret injection. It is **masked** inside
the sandbox, so the agent cannot read it even though it lives under `.cowboy/`.

```yaml
version: 1
sandbox:
  workdir: /workspace
  mounts:
    - { source: ".", target: /workspace, mode: rw }
  memory: 8g                     # or `auto` (¼ host RAM, 4g–16g); omit = unlimited
  cpus: 2                        # number or `auto` (½ host cores, 2–8); omit = unlimited
  host_tools: true               # expose ~/.local/bin, ~/.cargo/bin … read-only
  share_mise_store: true         # reuse your mise toolchains (copy-on-write)
network_policy:
  default_external: ask          # allow | deny | ask
  allow: { domains: [github.com], cidrs: [], ports: [80, 443] }
  deny:  { domains: [], cidrs: ["169.254.169.254/32"] }
  dns:   { enforce: true }       # strict allowlist + tunnel detection — see Network egress
secrets:
  env:
    - { name: GITHUB_TOKEN, source_env: COWBOY_GITHUB_TOKEN, required: false }
  files:                         # grant host credentials so CLIs work in the sandbox
    - { source: ~/.config/gh, target: /home/agent/.config/gh, read_only: true }
```

> **Renamed.** This section used to be `container:`. If yours still says that,
> cowboy refuses to load it and tells you to rename it — rather than silently
> ignoring the section and dropping every mount under it. Delete `image`,
> `dockerfile`, `build`, `privileged` and `docker_socket`, which no longer do
> anything; `workdir`, `mounts`, `memory` and `cpus` carry over unchanged.
>
> The `networks:` section is gone too. `isolated.enabled` toggled the gateway
> container; isolation is now unconditional — the sandbox's network namespace is
> connected to nothing — so a key claiming to turn it off would be a lie.
> `compose.approved` let the agent join a Docker network, which no longer exists.

**Mounts.** `sandbox.mounts` sources expand a leading `~` and `${VAR}` (like
`secrets.files`). Use this for paths a project *always* needs. For one-off access,
prefer `cowboy grant <path>` or let the agent ask with `request_path` — both take
effect on the next command with no restart, so `security.yaml` stays a statement of
intent rather than a scratchpad.

The sandbox's `HOME` is `/home/agent`, an ordinary confined home. It is **not** in
the workspace: it is backed by `~/.cache/cowboy/home/<project-key>` on the host, so
caches stay warm between sessions without putting anything in your repo — and
nothing the agent writes to `~` can be committed by accident. It is keyed by the
repository, so every worktree shares one warm cache.

To reuse your host package-manager caches instead of re-downloading, mount them
onto the XDG paths under it — e.g. `~/.local/share/pnpm → /home/agent/.local/share/pnpm`
(rw, so new packages cache back).

**Paths that can never be mounted or granted.** Credential stores (`~/.aws`,
`~/.ssh`, `~/.gnupg`, browser profiles, keyrings, …), `providers.yaml`, and the
cowboy config dir. Run `cowboy sandbox plan` to see the full list for your machine.
Use `cowboy secrets add` when a CLI genuinely needs credentials.

**Network policy / DNS.** The full allow/deny/ask model — and the DNS sub-policy
(`network_policy.dns`: strict allowlist gating, tunnel detection, allowed record
types) — is documented in [Network egress](../security/network.md).

**Granting credentials.** `secrets.env` injects an env var, either from a host env
var (`source_env`) or from a host command's output (`source_command`, like
`gh auth token`). A `source_command` runs on the host, in the project root, before a
command whenever its last value is over a minute old, so a short-lived token stays
fresh for the whole session; only its trimmed output enters the sandbox.
`secrets.files` mounts a host credential dir/file **read-only** under the agent's
`HOME` (`/home/agent/...`) so a CLI (`gh`, `kubectl`, …) works inside the sandbox. A
dotfile target under `/tmp`, the old `HOME`, is moved there. The credential *value*
never lands in config. `cowboy secrets add <preset>` writes ready-made grants — see
the [how-to](../how-to.md).

**Resource limits.** `cpus`/`memory` are enforced with an unprivileged cgroup v2
(a number/size, `auto` to size from the host, or omit for unlimited). They bound the
whole session, and they protect the machine rather than the boundary — the sandbox
confines correctly without them. Each session gets its **own** cgroup, so several
sessions in one project (a foreman and its subagents, say) each get the full ceiling
rather than sharing one between them.

`cpus` also **bounds build parallelism**. Modern `nproc` does read the CPU quota,
as do Rust's `available_parallelism` and the JVM, but plenty of tools do not (Node's
`os.cpus()` reports every host core), and a build sized for 32 cores under a 2-core
quota thrashes rather than failing. So Cowboy also injects `MAKEFLAGS=-j{cpus}`
(and `MAKE_OPTS`, `CARGO_BUILD_JOBS`, `npm_config_jobs`,
`CMAKE_BUILD_PARALLEL_LEVEL`, `MISE_JOBS`). The default `cpus: 2` keeps `8g`
comfortable; raise both (or use `auto`) for heavier builds. If a build is killed
with `exit 137`, it hit the memory ceiling — give it more.

Enforcement needs a delegated cgroup v2 subtree, which a systemd user session
provides. Without one the ceilings do not apply: `cowboy doctor` warns, and
`cowboy sandbox plan` marks them `NOT ENFORCED`, rather than reporting a limit that
is not in force.

**Session lifecycle & memory.** There's one sandbox per worktree, and they cost
close to nothing when idle:

- An **idle sandbox holds a single small holder process** and nothing else.
  `cpus`/`memory` are *caps*, not reservations, so the RAM you actually pay for is
  the dev processes the agent runs (servers, builds, language servers).
- **Ended sessions are reaped automatically** — the namespaces, the interception
  ruleset and the cgroup all go with the session's holder process, including after
  a crash: the holder's lifetime is tied to the worker's, so nothing outlives it.
- **Idle detached sessions free their RAM** — a detached session with no attached
  client tears its sandbox down after `agent.idle_sandbox_timeout_seconds`
  (default 30 min; `0` disables); the next command brings it back. The session
  stays resumable. A restarted sandbox gets brand-new namespaces, so enforcement is
  installed fresh rather than reusing anything.

A background process started by the agent (`processes` in `agent.yaml`) keeps the
filesystem view it started with, because a Landlock domain is fixed at `exec` and
can only narrow. Grant a path afterwards and Cowboy tells you which running
processes cannot see it, so restart those with `cowboy proc restart`.

## `agent.yaml` (mounted, agent-editable)

Non-security behavior only. This file is authoritative for execution when present:
Cowboy uses built-in defaults only when `agent.yaml` is absent. If the file exists
but is unreadable or malformed, session launch fails with the configuration error
rather than silently running with defaults. The taskless TUI launchpad is different:
its suggested prompts are best-effort UI hints, so it falls back to generic
suggestions when it cannot read this file; the worker still performs the strict load
before executing anything.

```yaml
version: 1
agent:
  command_timeout_seconds: 600
  model_timeout_seconds: 120
  idle_sandbox_timeout_seconds: 1800   # tear down an idle detached session's sandbox (0 = off)
  max_iterations: 100                    # turns per message when nobody is watching (see note)
  session_max_iterations: 500            # turns per message before your session asks to continue
  max_command_output_bytes: 60000
  project_instruction_bytes: 12000       # bytes of AGENTS.md pinned into the prompt (0 = off)
  setup:                                 # repo setup, run once per worktree (after mise install)
    - mise run sync
  verify:                                # checks that must pass before `final` (empty = no gate)
    - test
processes:                               # background processes the agent can start with `proc`
  web: { command: "npm run dev", cwd: /workspace, auto_start: false }
commands:                                # named shortcuts; shown to the agent
  test: cargo test
  lint: cargo clippy
```

**Project commands and verification.** `commands` is a map of named shortcuts. The
agent is shown it, so it runs *your* test and lint invocations rather than guessing
one from the language. `agent.verify` lists the checks that must have passed — each
entry is either a `commands` key or a literal command — before the agent may finish
a session that changed files; unverified edits get `final` refused with the exact
command to run. Evidence comes from real exit codes recorded host-side, and any
later edit invalidates it. Empty by default, which means no gate. It is a quality
mechanism, not a security control: it yields after a couple of refusals rather than
wedge a session whose checks cannot run. See
[Verification](../using/agent-and-tools.md#verification).

**Project instructions.** `agent.project_instruction_bytes` is how much of the repo's
root `AGENTS.md` (or `CLAUDE.md`) is pinned into the agent's system message, so it
starts knowing your conventions instead of spending a turn reading them — and still
knows them after compaction. Bounded because it is paid for on every request of every
session, subagents included; a longer file is clipped with a note to read the rest.
Set `0` to turn it off. See
[what the agent starts a session knowing](../using/agent-and-tools.md#what-the-agent-starts-a-session-knowing).

**Background processes.** `processes` names the long-running things a session may
need — a dev server, a watcher. The agent starts one with its `proc` tool (`proc
start web`, no command needed), `auto_start: true` brings it up before the first
turn, and output goes to `.cowboy/proc/<name>.log`. A process belongs to the session
and is reaped with it. See
[Background processes](../using/agent-and-tools.md#background-processes).

**Iteration budgets.** They exist to stop a runaway loop you can't see, so they
are tight where nobody is watching and loose where you are.

- **Your interactive session** gets `agent.session_max_iterations` (500) turns per
  message. When they run out, cowboy **asks whether to keep going** rather than
  stopping silently — answer yes and it gets another round and carries on in the
  same turn, up to ten extensions. While you're attached it doesn't nag the model to
  "start converging" on the way there; it just asks at the end.
- **`/budget off`** (TUI or web) turns the check off for the rest of the session, for
  a long task you're willing to let run; `/budget on` restores it, and `/budget`
  shows which is in force. It applies mid-turn. The loop's repeated-command and churn
  guards still apply either way.
- **Anything unattended** gets the converge nudges at 70% and 90% and is never
  extended: silence is not consent, so it ends the turn, and sending a message
  resumes with the conversation intact either way. A piped `cowboy "…"` run is held
  to `agent.max_iterations` (100); a session you've detached from keeps its session
  budget but stops at the end of it rather than extending.

A **delegated** worker does not use it: it gets a small grant sized by
`effort` and must report progress to earn more, bounded by a host-enforced ceiling.
Those knobs live in the crew roster — see
[Turn grants](../using/crew.md#turn-grants-report-progress-request-more).

**Startup setup.** When a session comes up, cowboy eagerly (before the first
message) brings the sandbox up and — if the repo uses [mise](https://mise.jdx.dev)
— runs a visible `mise install`, then any `agent.setup` commands. Bring-up is
narrated (namespaces, interception, resource ceilings) so a slow first run doesn't
look like a hang. `setup` runs
**once per worktree** (a marker at `.cowboy/sessions/.worktree-setup`, gitignored,
keyed to the commands — change them and it re-runs; delete it to force one). It's
streamed to the UI and stays interruptible, so a slow setup never blocks ending the
session. Use it for the per-worktree bootstrap your repo needs (install all deps,
codegen, …) — e.g. `mise run sync`.

## Providers & models

Provider credentials and model definitions are split so that **credentials are
host-owned and the agent can never reach them.**

### `~/.config/cowboy/providers.yaml` (home-only, `0600`)

Endpoints and their credentials. This file lives only in your home dir — never in a
project, never reachable from the sandbox. Configure as many providers as you use —
a local gateway for open models beside a hosted API — and manage them with
`cowboy providers`:

```sh
cowboy providers                      # list: endpoint, API, auth, models per provider
cowboy providers add bifrost --base-url http://localhost:8080/v1 --api responses --no-key
cowboy providers add openai --base-url https://api.openai.com/v1   # prompts for the key
cowboy providers remove bifrost       # refused while models or crew slots still use it
```

`cowboy models setup` also adds one, or reuses an existing one, when you add a model.

### Importing models from a catalogue

Rather than writing `models.yaml` by hand, import from a provider's catalogue:

```sh
cowboy models import <provider>              # asks: all, a pattern, or pick from a list
cowboy models import <provider> --match 'glm*' --yes
cowboy models refresh [<provider>]           # update context windows from the catalogue
```

Each imported model is named `provider/<last part of its id>` (the whole id, with `/`
turned into `-`, if that name is taken). Image, audio and embedding models are
skipped, and so are models already configured on that provider. Values come from:

| Field | Source |
|---|---|
| `context_window` | the catalogue, when it reports one; else Cowboy's shipped table |
| `reasoning_effort` | the catalogue's default level, else the shipped table — never a level the catalogue says the model refuses |
| `reasoning_levels` | the levels the catalogue says the model accepts (the ChatGPT catalogue lists them; most gateways don't) |
| `max_tokens`, `temperature`, prices | the shipped table (`max_tokens` capped at half the window) |

`refresh` changes only the context window, the accepted levels, and the reasoning
level when yours is one the model refuses. Prices, temperature and output caps are yours and stay. Models a
catalogue no longer lists are reported, not removed. `providers add`, `models setup`
and a ChatGPT sign-in all end by offering an import.

```yaml
version: 1
providers:
  litellm:
    base_url: https://your-openai-compatible-endpoint/v1   # supports ${VAR}
    api_key: sk-...                                         # stored literally; file is 0600
    headers: {}                                             # optional
  bifrost:
    base_url: http://localhost:8080/v1
    api: responses             # chat_completions (default) | responses
                               # no api_key: a keyless local gateway
  chatgpt:
    base_url: https://api.openai.com/v1
    api: responses
    auth: chatgpt              # api_key (default) | chatgpt — see below
```

`api` selects the wire protocol: `chat_completions` (`POST /chat/completions`, what
nearly every OpenAI-compatible endpoint speaks) or `responses` (the OpenAI Responses
API). A file that uses neither new field is written back exactly as an older Cowboy
expects; one that sets `api` (or `auth`) cannot be read by an older binary.

`auth: chatgpt` uses your ChatGPT subscription instead of a key: sign in with
`cowboy providers add chatgpt --chatgpt` (or `cowboy providers login <name>`), and the
OAuth tokens live in their own `0600` record under `~/.config/cowboy/auth/`. It
requires `api: responses` and takes no `api_key`; both mistakes are refused when the
file loads. See [ChatGPT sign-in](../using/chatgpt.md).

### `models.yaml` — user (`~/.config/cowboy/`) and/or project (`.cowboy/`)

Models are grouped under the provider that serves them, and everything that names a
model — `default`, `summarizer`, the [crew roster](../using/crew.md), `/model`,
`cowboy models use` — names it as **`provider/name`**. The same name under two
providers is two different models. The file **never contains credentials** (a stray
`api_key`/`base_url` is a hard parse error), and a project file can only name
providers defined in your home `providers.yaml`; it can never define one.

User and project files merge by `provider/name` (project wins); the default is
`project.default`, falling back to `user.default`.

```yaml
version: 1
default: litellm/sonnet
summarizer: litellm/haiku   # optional: model for summaries (compaction + recovery)
models:
  litellm:                   # a provider from providers.yaml
    sonnet:                  # → litellm/sonnet
      model: anthropic/claude-sonnet-4-6   # the provider-side id (may contain `/`)
      temperature: 0.2
      max_tokens: 32768          # max OUTPUT tokens per response (see note)
      context_window: 1000000    # total input+output window the model supports
      input_cost_per_mtok: 3.0   # optional, for usage/cost display
      output_cost_per_mtok: 15.0
      cached_input_cost_per_mtok: 0.30  # optional: prompt-cache hit price (see below)
      anthropic_cache: true      # optional: see below
      reasoning_effort: medium   # optional: the level sent (unset: none sent)
      reasoning_levels: [low, medium, high]   # optional: the levels the model accepts
```

**Reasoning levels.** `reasoning_effort` is one of `minimal`, `low`, `medium`,
`high`, `xhigh` and `max`; leave it out and no level is sent, so the provider's
default applies. OpenAI's catalogue also lists `ultra`, which it describes as
reasoning with automatic task delegation. That overlaps the
[crew](../using/crew.md), so Cowboy reads `ultra` as `max`. When `reasoning_levels`
is set (an import records it), every level asked for — this setting, a session's
`/reasoning`, or a crew slot's `@level` — is stepped to one the model accepts: down
to the highest accepted level below it, or up to the lowest when it is below them
all. Without the list, a level is sent as asked. A session can change its level with
`/reasoning` (see [Everyday use](../how-to.md#reasoning-levels)).

Provider and model names may not contain `/`; the provider-side `model:` id can.

**Upgrading from bare model names.** Before models were grouped by provider, each
entry carried a `provider:` field and was referred to by its bare name. Cowboy now
refuses that layout (and bare names in `crew.yaml`) rather than guessing which
provider you meant. Convert once:

```sh
cowboy models migrate    # shows a diff, asks, keeps a .bak of every file it rewrites
```

It rewrites your home `models.yaml`, the current project's `.cowboy/models.yaml`, and
`crew.yaml` (in place, so roster comments survive). A name that was the provider-side
id — `fireworks/accounts/fireworks/models/glm-5p3` — becomes its last segment,
`default/glm-5p3`, unless that would collide. An older `cowboy` binary cannot read
the migrated files.

**`context_window` vs `max_tokens`.** `context_window` is the model's *total*
window (prompt + completion); Cowboy prunes history to fit it. `max_tokens` is the
cap on a *single response's output* — not always 8192. Tune it to the model's real
max output (e.g. Claude Sonnet 4.6 ≈ 64k, Opus 4.8 ≈ 128k) but keep it a sane
agent cap (16k–32k is a good sweet spot — enough for a long file/edit without
letting one response run away).

Cowboy reserves `max_tokens` from the window for the answer, plus the tool schemas
(~3.6k tokens, sent on every request) and a small headroom floor; what remains is the
budget the conversation may occupy. So the two settings interact: a large `max_tokens`
against a modest `context_window` leaves little room for history and makes compaction
frequent. `/context` shows the split for the current session, and if the window cannot
hold the reserve at all Cowboy tells you which number to change rather than letting the
request fail at the provider.

**`summarizer`** (optional): names a model used for Cowboy's internal
summarization — folding old history into a summary when the context window fills,
and **truncation recovery**. When a reasoning model spends its whole `max_tokens`
budget thinking and emits no answer or tool call, Cowboy warns that the output limit
may be too low and retries the turn asking it to answer now, at the lowest reasoning
level the model accepts (`minimal` when `reasoning_levels` doesn't say); if the
cut-off reasoning came back, it is first distilled into conclusions-so-far to build
on. Bounded, so a model that always
truncates can't spin — see [the agent loop](../using/agent-and-tools.md). Point
`summarizer` at a small/cheap model to make these auxiliary calls faster and cheaper;
when unset, the session's main model is used. These calls always request the lowest
reasoning level the model accepts: summarizing is mechanical, and a model that just
truncated while thinking would otherwise do the same on the summary and come back
empty.

**Cost display.** Cowboy asks the provider for per-request token usage
(`stream_options.include_usage`) and, when the stream carries it, bills the
session from those counts — the billing ground truth — rather than its local
tokenizer estimate. Providers that support prompt caching report cache hits
(`cached_tokens`), which are priced at `cached_input_cost_per_mtok`.

**Set that rate, or the figure will be far too high.** An agent re-sends a large
cached prefix on nearly every request, so for a typical session *almost all* input
tokens are cache reads — 99% is normal. Cache reads are also much cheaper than
fresh input, and by more than you would guess: measured against one provider's own
billing, 3% of the input price for DeepSeek V4.1 Flash, 19% for GLM 5.3, 10–13%
for Kimi K3 and Qwen 3.8 Max. With no rate configured Cowboy falls back to the
full input price, which never *understates* spend but overstated one real session
**7.5×** ($18.70 shown against a $2.50 bill).

Cowboy fills the gap two ways. Models in its shipped table get their cache rate
automatically, even for a `models.yaml` entry written before the rate was known —
so most users need do nothing. For a model it does not know, it says so once per
session ("cost is overstated: N% of prompt tokens are cache reads…") rather than
guessing a discount, because the discount varies too much between models to guess.
To get the number exactly right, take the cached-input price from your provider's
pricing page and set `cached_input_cost_per_mtok`.

When a provider reports no usage at all, Cowboy falls back to the local estimate at
the full input/output rates, so the display can drift from the dashboard.

**`anthropic_cache`** (opt-in): when true, Cowboy adds Anthropic `cache_control`
markers to the static system prompt and the latest message, so a gateway that
understands Anthropic prompt caching reuses the cached prefix across turns (big
latency/cost win for Claude). Only enable it for Anthropic models behind a gateway
that supports `cache_control` — it's ignored or rejected elsewhere.

**`stream_idle_timeout_seconds`** (optional, default 300): abort a streaming
response if the provider sends *nothing* (not even an SSE keep-alive) for this
long — a silently stalled stream would otherwise hang the turn forever. Any bytes
on the wire reset the clock, so slow-but-alive models are unaffected. Set `0` to
disable, or raise it for models that think for long stretches without streaming.

**Fallback when a model disappears.** Providers retire and rename model ids. If the
model a session (or a crew-routed subagent) is running turns out not to exist at
the provider — a `404 model_not_found` — Cowboy reroutes **once** to the configured
`default` model, says so in the transcript, and journals a `model_fallback`
lifecycle event, rather than failing the session. This is a safety net, not a fix:
the notice tells you which id to correct in `models.yaml` / `crew.yaml`. Note that
the crew roster's own fallback is a *routing-time* choice, so it cannot help here —
only this runtime reroute can.

Manage with `cowboy models setup` / `list` / `use [-g] <name>`. Works with any
OpenAI-compatible backend. Cowboy does not manage or endorse a gateway.
