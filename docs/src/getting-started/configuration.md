# Configuration

Three files live under `.cowboy/`. `cowboy init` writes commented defaults. A
deeper field reference is in [Config files](../reference/config-files.md).

## `security.yaml` (host-owned, never mounted)

Read only by the host `cowboy` process. Controls the container, mounts, networks,
network policy, and secret injection. It is **masked** inside the container, so
the agent cannot read it even though it lives under `.cowboy/`.

```yaml
version: 1
container:
  image: cowboy/agent:local      # built locally on first run
  workdir: /workspace
  mounts:
    - { source: ".", target: /workspace, mode: rw }
  privileged: false              # doctor warns if true
  docker_socket: false           # doctor warns if true
  memory: 8g
  cpus: 4
networks:
  isolated: { enabled: true }    # bring up the sole-egress gateway
  compose:  { approved: [] }     # Docker/Compose networks the agent may join
network_policy:
  default_external: ask          # allow | deny | ask
  allow: { domains: [github.com], cidrs: [], ports: [80, 443] }
  deny:  { domains: [], cidrs: ["169.254.169.254/32"] }
secrets:
  env:
    - { name: GITHUB_TOKEN, source_env: COWBOY_GITHUB_TOKEN, required: false }
```

## `agent.yaml` (mounted, agent-editable)

Non-security behavior only.

```yaml
version: 1
agent:
  command_timeout_seconds: 600
  model_timeout_seconds: 120
  max_iterations: 100
  max_command_output_bytes: 60000
processes:
  web: { command: "npm run dev", cwd: /workspace, auto_start: false }
commands:
  test: cargo test
```

## Providers & models

Provider credentials and model definitions are split so that **credentials are
host-owned and the agent can never reach them.**

### `~/.config/cowboy/providers.yaml` (home-only, `0600`)

Endpoint + key pairs. This file lives only in your home dir — never in a project,
never mounted into the container. Manage it with `cowboy models setup`.

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
project lists merge by name (project wins); the default is `project.default` ??
`user.default`.

```yaml
version: 1
default: sonnet
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

**`anthropic_cache`** (opt-in): when true, Cowboy adds Anthropic `cache_control`
markers to the static system prompt and the latest message, so a gateway that
understands Anthropic prompt caching reuses the cached prefix across turns (big
latency/cost win for Claude). Only enable it for Anthropic models behind a gateway
that supports `cache_control` — it's ignored or rejected elsewhere.

Manage with `cowboy models setup` / `list` / `use [-g] <name>`. Works with any
OpenAI-compatible backend. Cowboy does not manage or endorse a gateway.
