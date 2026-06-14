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
cargo install --path crates/cowboy-cli   # installs `cowboy` to ~/.cargo/bin
docker/build.sh                          # build the agent + gateway images
cowboy models setup                      # save a provider (endpoint + key) to ~/.config/cowboy
cd your-project
cowboy init                              # writes .cowboy/{security,agent}.yaml
cowboy doctor                            # check Docker, Linux, nft, model, Compose
cowboy "run the tests and fix one simple failure"
```

**Providers vs. models.** Provider credentials (endpoint URL + API key) are
host-owned: `cowboy models setup` saves them to `~/.config/cowboy/providers.yaml`
(`0600`), never in a project, so the agent can't reach them. Models (which
provider + model id + params) can be defined at the user level
(`~/.config/cowboy/models.yaml`) or per project (`.cowboy/models.yaml`, no
credentials); set the default with `cowboy models use [-g] <name>` and review with
`cowboy models list`.

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
cargo nextest run                   # unit + integration (Docker E2E auto-skips if absent)
cargo test --doc                    # doctests (nextest doesn't run these)
cargo test -- --ignored gateway     # the full network-boundary E2E (builds the gateway image)
cargo clippy --workspace --all-targets

# Coverage (cargo-llvm-cov). On a rustup toolchain `llvm-tools-preview` is used
# automatically; on a system-LLVM toolchain point it at the matching version:
LLVM_COV=/usr/lib/llvm/<v>/bin/llvm-cov \
LLVM_PROFDATA=/usr/lib/llvm/<v>/bin/llvm-profdata \
  cargo llvm-cov nextest --summary-only
```

Build the container images: `docker/build.sh` (or `docker/build.sh agent|gateway`).
