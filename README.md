# cowboy

[![CI](https://github.com/koshea/cowboy/actions/workflows/ci.yml/badge.svg)](https://github.com/koshea/cowboy/actions/workflows/ci.yml)

An opinionated local coding agent that lets the AI run wild inside a
Docker-contained development environment, while the **host** enforces security
at the container and network layer.

> The agent can run wild because the runtime owns the corral.

The agent is **not** part of the security boundary. Security is enforced by
Docker, host-owned configuration, and a Cowboy-controlled network gateway —
never by prompting the model.

## Quick start

```sh
cargo install --git https://github.com/koshea/cowboy cowboy-cli   # installs `cowboy` + `cowboyd`
cowboy models setup                      # save a provider (endpoint + key) to ~/.config/cowboy
cd your-project
cowboy init                              # writes .cowboy/{security,agent}.yaml
cowboy doctor                            # check Docker, Linux, nft, model, Compose
cowboy "run the tests and fix one simple failure"
```

The agent + gateway images are **pulled from GHCR on first run**, pinned to your
binary's version (`ghcr.io/koshea/cowboy/{agent,gateway}`) — no image build step.

**Providers vs. models.** Provider credentials (endpoint URL + API key) are
host-owned: `cowboy models setup` saves them to `~/.config/cowboy/providers.yaml`
(`0600`), never in a project, so the agent can't reach them. Models (which
provider + model id + params) can be defined at the user level
(`~/.config/cowboy/models.yaml`) or per project (`.cowboy/models.yaml`, no
credentials); set the default with `cowboy models use [-g] <name>` and review with
`cowboy models list`.

## Docs

Full documentation lives at **[cowboycode.io](https://cowboycode.io)**.

Highlights: [Quick start](https://cowboycode.io/getting-started/quickstart.html) ·
[Security model](https://cowboycode.io/security/model.html) ·
[Network gateway](https://cowboycode.io/security/network.html) ·
[Configuration](https://cowboycode.io/getting-started/configuration.html) ·
[Ranch Plans](https://cowboycode.io/ranch/overview.html) ·
[CLI reference](https://cowboycode.io/reference/cli.html).

The site is an [mdBook](https://rust-lang.github.io/mdBook/) built from
[`docs/`](docs/src/SUMMARY.md) and published on every push to `main`.

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

Install from a checkout (`cargo install --path crates/cowboy-cli`) and cowboy
builds the agent/gateway images from *your* source instead of pulling, so local
Dockerfile changes take effect automatically. `docker/build.sh` builds them
explicitly; `COWBOY_SRC=/path/to/cowboy` forces source builds from any binary.

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

Preview the docs locally with `mdbook serve docs` (`cargo install mdbook` once) —
live at <http://localhost:3000>.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full contributor guide and
[SECURITY.md](SECURITY.md) for reporting vulnerabilities.

## License

Licensed under the [MIT License](LICENSE). © 2026 Kevin O'Shea (koshea).
