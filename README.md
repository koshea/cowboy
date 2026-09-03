# cowboy

[![CI](https://github.com/koshea/cowboy/actions/workflows/ci.yml/badge.svg)](https://github.com/koshea/cowboy/actions/workflows/ci.yml)

An opinionated local coding agent that lets the AI run wild in a sandbox built
from your own machine, while the **host** enforces security at the kernel and
network layer.

> The agent can run wild because the runtime owns the corral.

The agent is **not** part of the security boundary. Security is enforced by Linux
namespaces, Landlock, seccomp, host-owned configuration, and a policy engine that
runs in the host process — never by prompting the model.

The agent gets **your** toolchain, read-only: the compilers and CLIs you actually
installed, at your versions, with no image to build or pull. Need it to see a
folder outside the project? It asks, or you run `cowboy grant <path>`, and the next
command sees it — no restart.

## Quick start

```sh
cargo install --git https://github.com/koshea/cowboy cowboy-cli   # installs `cowboy` + `cowboyd`
cowboy models setup                      # save a provider (endpoint + key) to ~/.config/cowboy
cd your-project
cowboy init                              # writes .cowboy/{security,agent}.yaml
cowboy doctor                            # kernel prerequisites, model config, daemon
cowboy "run the tests and fix one simple failure"
```

The default install ships the CLI and daemon. To also embed the **`cowboy web`**
UI, set `COWBOY_WEB_UI=1` for the build — it then ensures `trunk` and the wasm
target are present and bundles the frontend into the binary (one-time, slower).
The `env …` prefix works in every shell (including fish/zsh, which reject the
bare `VAR=value cmd` form):

```sh
env COWBOY_WEB_UI=1 cargo install --git https://github.com/koshea/cowboy cowboy-cli
```

**Install a prebuilt `trunk` first.** If `trunk` isn't found the build will try
to install it, preferring a prebuilt binary (`cargo binstall`) but falling back
to building from source — and the source build pulls in `libdeflate-sys`, which
fails on bleeding-edge compilers (e.g. GCC 16). Save yourself the trouble by
installing a prebuilt `trunk` beforehand; with it on `PATH` the build reuses it:

```sh
sudo pacman -S trunk     # Arch        (brew install trunk on macOS;
rustup target add wasm32-unknown-unknown  #  or `cargo binstall trunk` anywhere)
```

Already installed at the same version? Add `--force` so cargo actually rebuilds
(it skips a same-commit reinstall otherwise, and the flag would have no effect).

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
[The boundary](https://cowboycode.io/security/model.html) ·
[Network egress](https://cowboycode.io/security/network.html) ·
[Sandbox design decisions](https://cowboycode.io/security/sandbox-decisions.html) ·
[Configuration](https://cowboycode.io/getting-started/configuration.html) ·
[Ranch Plans](https://cowboycode.io/ranch/overview.html) ·
[CLI reference](https://cowboycode.io/reference/cli.html).

The site is an [mdBook](https://rust-lang.github.io/mdBook/) built from
[`docs/`](docs/src/SUMMARY.md) and published on every push to `main`.

## Workspace layout

```
crates/
  cowboy-cli/      # the `cowboy` binary: CLI, agent loop, the sandbox, sessions
  cowboy-core/     # config, OpenAI-compatible model client, network policy, shared types
  cowboy-sandbox/  # the sandbox plan as pure logic (binds, Landlock, seccomp, denylist)
  cowboy-tui/      # ratatui rendering (snapshot-tested)
  cowboy-gateway/  # the policy engine: proxy, DNS, ip→domain attribution (a library)
docs/
```

## Requirements

**Linux only** — the sandbox is namespaces, Landlock, seccomp and nftables, and
there is no container or VM in the design to borrow them from.

- A kernel with **Landlock ABI 6+** (Linux 6.10+), `CONFIG_SECCOMP_FILTER`, and
  unprivileged user namespaces
- **bubblewrap** (non-setuid), `unshare`, `ip`, `nft`
- An OpenAI-compatible model endpoint

`cowboy doctor` checks each of these *by performing it* and names the kernel option
or package to change for anything missing.

## Development

Install from a checkout with `cargo install --path crates/cowboy-cli`.

```sh
cargo nextest run                   # unit + integration
cargo test --doc                    # doctests (nextest doesn't run these)
cargo clippy --workspace --all-targets

# The sandbox suites self-skip when the host can't run them, which can hide a
# broken probe. `required` turns a skip into a failure — always verify with it.
COWBOY_SANDBOX_TESTS=required cargo nextest run -p cowboy-cli \
    --test sandbox_exec --test sandbox_session --test sandbox_egress

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
