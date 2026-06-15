# Installation

## Requirements

- **Linux** (the current target).
- **Docker** and **`docker compose`**.
- **`nftables`** (the gateway applies an nft ruleset).
- An **OpenAI-compatible model endpoint** (see below).

`cowboy doctor` checks all of these and reports what's missing.

## Recommended: an LLM gateway

Cowboy talks to one OpenAI-compatible endpoint, and by design it does **not**
handle quotas, rate limits, spend caps, retries, or failover — those belong to an
**LLM gateway** in front of your providers. Before you get started, the smoothest
setup is to stand one up:

- **[LiteLLM](https://github.com/BerriAI/litellm)** or
  **[Bifrost](https://github.com/maximhq/bifrost)** — point Cowboy at the gateway,
  and define as many backend models as you like behind it (different providers,
  fallbacks, budgets, keys) without changing anything in Cowboy.

A gateway is what makes the [crew](../using/crew.md) shine: your roster names
logical models, the gateway resolves them — including routing aliases and
failover — and owns the rate/quota/spend policy.

**Just want one provider?** That's fine too — Cowboy works with any single
OpenAI-compatible endpoint directly (OpenAI, OpenRouter, a local Ollama or vLLM,
an internal gateway). You don't need to run a gateway to start; reach for one when
you want multiple models, budgets, or failover.

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

Provider credentials are **host-owned** and stored outside any project. Point the
endpoint at your gateway (if you set one up) or directly at a provider:

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
