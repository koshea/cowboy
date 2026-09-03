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
networks:
  isolated: { enabled: true }
network_policy:
  default_external: ask          # allow | deny | ask
  allow: { domains: [github.com], cidrs: [], ports: [80, 443] }
  deny:  { domains: [], cidrs: ["169.254.169.254/32"] }
  dns:   { enforce: true }       # strict allowlist + tunnel detection — see Network egress
secrets:
  env:
    - { name: GITHUB_TOKEN, source_env: COWBOY_GITHUB_TOKEN, required: false }
  files:                         # grant host credentials so CLIs work in the sandbox
    - { source: ~/.config/gh, target: /tmp/.config/gh, read_only: true }
```

> **Renamed.** This section used to be `container:`. If yours still says that,
> cowboy refuses to load it and tells you to rename it — rather than silently
> ignoring the section and dropping every mount under it. `image`, `dockerfile` and
> `build` no longer do anything and can be deleted.

**Mounts.** `sandbox.mounts` sources expand a leading `~` and `${VAR}` (like
`secrets.files`). Use this for paths a project *always* needs. For one-off access,
prefer `cowboy grant <path>` or let the agent ask with `request_path` — both take
effect on the next command with no restart, so `security.yaml` stays a statement of
intent rather than a scratchpad.

The sandbox's `HOME` is `{workdir}/.cowboy/home`, an ordinary confined home. To
reuse your host package-manager caches instead of re-downloading, mount them onto
the XDG paths under it — e.g.
`~/.local/share/pnpm → /workspace/.cowboy/home/.local/share/pnpm` (rw, so new
packages cache back).

**Paths that can never be mounted or granted.** Credential stores (`~/.aws`,
`~/.ssh`, `~/.gnupg`, browser profiles, keyrings, …), `providers.yaml`, and the
cowboy config dir. Run `cowboy sandbox plan` to see the full list for your machine.
Use `cowboy secrets add` when a CLI genuinely needs credentials.

**Network policy / DNS.** The full allow/deny/ask model — and the DNS sub-policy
(`network_policy.dns`: strict allowlist gating, tunnel detection, allowed record
types) — is documented in [Network egress](../security/network.md).

**Granting credentials.** `secrets.env` injects an env var (from a host env var or
a `source_command` like `gh auth token`); `secrets.files` mounts a host credential
dir/file **read-only** so a CLI (`gh`, `gcloud`, `kubectl`, …) works inside the
sandbox. The credential *value* never lands in config.
`cowboy secrets add <preset>` prints ready-to-paste grants — see the
[how-to](../how-to.md).

**Resource limits.** `cpus`/`memory` are enforced with an unprivileged cgroup v2
(a number/size, `auto` to size from the host, or omit for unlimited). They bound the
whole session, and they protect the machine rather than the boundary — the sandbox
confines correctly without them.

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

Non-security behavior only.

```yaml
version: 1
agent:
  command_timeout_seconds: 600
  model_timeout_seconds: 120
  idle_sandbox_timeout_seconds: 1800   # tear down an idle detached session's sandbox (0 = off)
  max_iterations: 100
  max_command_output_bytes: 60000
  setup:                                 # repo setup, run once per worktree (after mise install)
    - mise run sync
processes:
  web: { command: "npm run dev", cwd: /workspace, auto_start: false }
commands:
  test: cargo test
```

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

Endpoint + key pairs. This file lives only in your home dir — never in a project,
never reachable from the sandbox. Manage it with `cowboy models setup`.

```yaml
version: 1
providers:
  litellm:
    base_url: https://your-openai-compatible-endpoint/v1   # supports ${VAR}
    api_key: sk-...                                         # stored literally; file is 0600
    headers: {}                                             # optional
```

### `models.yaml` — user (`~/.config/cowboy/`) and/or project (`.cowboy/`)

A model names a provider plus the model id and sampling params. **Never contains
credentials** (a stray `api_key`/`base_url` is a hard parse error). User and
project lists merge by name (project wins); the default is `project.default`, falling back to
`user.default`.

```yaml
version: 1
default: sonnet
summarizer: haiku        # optional: model for summaries (compaction + recovery)
models:
  sonnet:
    provider: litellm
    model: anthropic/claude-sonnet-4-6
    temperature: 0.2
    max_tokens: 32768          # max OUTPUT tokens per response (see note)
    context_window: 1000000    # total input+output window the model supports
    input_cost_per_mtok: 3.0   # optional, for usage/cost display
    output_cost_per_mtok: 15.0
    anthropic_cache: true      # optional: see below
```

**`context_window` vs `max_tokens`.** `context_window` is the model's *total*
window (prompt + completion); Cowboy prunes history to fit it. `max_tokens` is the
cap on a *single response's output* — not always 8192. Tune it to the model's real
max output (e.g. Claude Sonnet 4.6 ≈ 64k, Opus 4.8 ≈ 128k) but keep it a sane
agent cap (16k–32k is a good sweet spot — enough for a long file/edit without
letting one response run away). Cowboy reserves `max_tokens` of the window for the
answer when pruning, so `prompt + output` never exceeds `context_window`; setting
it accurately keeps requests valid even when the context is nearly full.

**`summarizer`** (optional): names a model used for Cowboy's internal
summarization — folding old history into a summary when the context window fills,
and **truncation recovery**. When a reasoning model spends its whole `max_tokens`
budget thinking and emits no answer or tool call, Cowboy warns that the output
limit may be too low, distills the cut-off reasoning into conclusions-so-far, and
retries the turn with a directive to act on them (bounded, so a model that always
truncates can't spin). Point `summarizer` at a small/cheap model to make these
auxiliary calls faster and cheaper; when unset, the session's main model is used.

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
