# Installation

## Requirements

- **Linux** or **macOS** (Docker Desktop). The network gateway runs as a sidecar
  inside the agent's container netns, so the host needs no `nftables` itself — the
  enforcement uses the Docker (VM) kernel's netfilter.
- **Docker** and **`docker compose`**. On macOS, Docker Desktop (its Linux VM is
  where containers and the gateway run).
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

Install straight from GitHub (needs a Rust toolchain — [rustup](https://rustup.rs)):

```sh
cargo install --git https://github.com/koshea/cowboy cowboy-cli
```

This builds and installs `cowboy` (and the `cowboyd` daemon) to `~/.cargo/bin`.

## Container images

You don't build anything. On first run, cowboy **pulls** its agent and gateway
images from GHCR, **pinned to your binary's version** so they always match:

- `ghcr.io/koshea/cowboy/agent:<version>` (multi-arch: amd64 + arm64)
- `ghcr.io/koshea/cowboy/gateway:<version>`

Upgrading the binary (`cargo install --git … --force`) moves you to the matching
images automatically. To customize the agent image, commit a
[`.cowboy/Dockerfile`](../how-to.md) (`FROM` the base) — it's built per-repo on top
of the pulled base, so contributors share it without any local image work.

## Developing cowboy

If you've **cloned the repo**, cowboy builds the images from *your* source instead
of pulling — so local Dockerfile changes take effect with no extra steps:

```sh
git clone https://github.com/koshea/cowboy && cd cowboy
cargo install --path crates/cowboy-cli   # source-installed binary auto-builds local images
```

- `docker/build.sh [agent|gateway]` builds them explicitly (tagged with the
  version-pinned names, so the binary picks them up without pulling).
- `COWBOY_SRC=/path/to/cowboy` forces source builds from any binary.

A binary installed via `cargo install --git` is treated as an *end user* (it pulls),
even though cargo caches the checkout under `~/.cargo`.

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
cowboy doctor                   # platform, Docker, model config, gateway image, Compose
```
