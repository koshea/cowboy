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
    max_tokens: 8192
    context_window: 200000
```

Manage with `cowboy models setup` / `list` / `use [-g] <name>`. Works with any
OpenAI-compatible backend. Cowboy does not manage or endorse a gateway.
