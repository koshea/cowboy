# cowboy — configuration

Three files live under `.cowboy/`. `cowboy init` writes commented defaults.

## `security.yaml` (host-owned, never mounted)

Read only by the host `cowboy` process. Controls the container, mounts,
networks, network policy, and secret injection.

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

## `models.yaml` (host-owned)

OpenAI-compatible model profiles. **The API key is read from the env var named
by `api_key_env` — never store the key in this file.**

```yaml
version: 1
models:
  default: dev
  profiles:
    dev:
      base_url: https://your-openai-compatible-endpoint/v1
      api_key_env: COWBOY_OPENAI_API_KEY
      model: anthropic/claude-sonnet-4-6
      temperature: 0.2
      max_tokens: 8192
      context_window: 200000
```

Works with any OpenAI-compatible backend (LiteLLM, OpenRouter, Ollama, vLLM, an
internal gateway, …). Cowboy does not manage or endorse a gateway.
