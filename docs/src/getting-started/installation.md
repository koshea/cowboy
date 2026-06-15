# Installation

## Requirements

- **Linux** (the current target).
- **Docker** and **`docker compose`**.
- **`nftables`** (the gateway applies an nft ruleset).
- An **OpenAI-compatible model endpoint** (LiteLLM, OpenRouter, Ollama, vLLM, an
  internal gateway, …).

`cowboy doctor` checks all of these and reports what's missing.

## Install the binaries

```sh
cargo install --path crates/cowboy-cli   # installs `cowboy` (and `cowboyd`) to ~/.cargo/bin
```

## Build the container images

The agent and gateway run from local images:

```sh
docker/build.sh                 # build both
docker/build.sh agent           # or just one
docker/build.sh gateway
```

The default agent image is `cowboy/agent:local`; the gateway is
`cowboy/gateway:local`. The agent image is also built automatically on first run
if missing.

## Configure a model provider

Provider credentials are **host-owned** and stored outside any project:

```sh
cowboy models setup             # save an endpoint + key to ~/.config/cowboy/providers.yaml (0600)
cowboy models list              # review providers + models
cowboy models use [-g] <name>   # set the default model (-g = global/user level)
```

See [Configuration](configuration.md) for how providers and models are split so
the agent can never reach your credentials.

## Verify

```sh
cowboy doctor                   # Docker, Linux, nft, model config, Compose
```
