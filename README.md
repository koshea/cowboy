# cowboy

An opinionated local coding agent that lets the AI run wild inside a
Docker-contained development environment, while the **host** enforces security
at the container and network layer.

> The agent can run wild because the runtime owns the corral.

The agent is **not** part of the security boundary. Security is enforced by
Docker, host-owned configuration, and a Cowboy-controlled network gateway —
never by prompting the model.

## Quick start

```sh
cargo build --release
export COWBOY_OPENAI_API_KEY=...        # key for your OpenAI-compatible endpoint
cd your-project
cowboy init                              # writes .cowboy/{security,agent,models}.yaml
cowboy doctor                            # check Docker, Linux, nft, model, Compose
cowboy "run the tests and fix one simple failure"
```

## Docs

- [docs/MVP.md](docs/MVP.md) — overview & command reference
- [docs/SECURITY.md](docs/SECURITY.md) — the security model
- [docs/CONFIG.md](docs/CONFIG.md) — the three config files
- [docs/NETWORK.md](docs/NETWORK.md) — the sole-egress network gateway

## Workspace layout

```
crates/
  cowboy-cli/      # the `cowboy` binary: CLI, agent loop, docker + gateway orchestration, session
  cowboy-core/     # config, OpenAI-compatible model client, network policy, shared types
  cowboy-tui/      # ratatui rendering (snapshot-tested)
  cowboy-gateway/  # the sole-egress gateway binary (proxy + DNS + nft policy)
docker/            # agent + gateway images
docs/
```

## Requirements

- Linux (the MVP is Linux-only)
- Docker, `docker compose`, and `nftables` (`cowboy doctor` checks these)
- An OpenAI-compatible model endpoint

## Development

```sh
cargo test                          # unit + integration (Docker E2E auto-skips if absent)
cargo test -- --ignored gateway     # the full network-boundary E2E (builds the gateway image)
cargo clippy --workspace --all-targets
```
