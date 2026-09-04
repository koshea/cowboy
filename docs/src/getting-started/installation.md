# Installation

## Requirements

**Linux only.** The sandbox is built from Linux kernel features — namespaces,
Landlock, seccomp, nftables — and there is no container or VM to run it in
elsewhere.

- A kernel providing **Landlock ABI 6 or newer** (Linux 6.10+), with
  `CONFIG_SECURITY_LANDLOCK` and `landlock` in `CONFIG_LSM`.
- **`CONFIG_SECCOMP_FILTER`** and **unprivileged user namespaces**
  (`CONFIG_USER_NS`, with `sysctl user.max_user_namespaces` above zero).
- **bubblewrap** (`bwrap`), and it must *not* be setuid — nothing here needs a
  privileged helper.
- **`unshare`** (util-linux), **`ip`** (iproute2), **`nft`** (nftables), `sysctl`.
- Optional but recommended: a **systemd user session**, which delegates the cgroup
  subtree used for memory/CPU/process ceilings. Without it the sandbox still
  confines correctly; the ceilings just do not apply.
- An **OpenAI-compatible model endpoint** (see below).

`cowboy doctor` checks every one of these *by performing it* — it creates a user
namespace, asks the kernel its Landlock ABI, and loads the real interception
ruleset into a throwaway namespace — and tells you the kernel option or package to
change for anything missing.

No Docker. No images to pull or build. The agent uses the toolchain already
installed on your machine, which is the point.

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

Every tagged release ships prebuilt `cowboy` + `cowboyd` for `x86_64` and `aarch64`
Linux, with the web UI already embedded and a `SHA256SUMS` file beside them:

```sh
tar xzf cowboy-<version>-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 cowboy-*/cowboy cowboy-*/cowboyd ~/.local/bin/
```

There is nothing else to fetch — no image, no runtime. Built against the glibc on
GitHub's `ubuntu-24.04` runners, so build from source below if yours is older.

Or build it yourself (needs a Rust toolchain — [rustup](https://rustup.rs)):

```sh
cargo install --locked --git https://github.com/koshea/cowboy cowboy-cli
```

This builds and installs `cowboy` (and the `cowboyd` daemon) to `~/.cargo/bin`.

`--locked` is worth keeping: it builds the dependency versions this project actually
tests, rather than re-resolving to whatever is newest today. Without it, cargo also
reports a couple of transitive crates as behind their latest release — `matchit` and
`generic-array`, both pinned to an exact version by `axum`, so there is nothing to
update and nothing wrong.

## Upgrading

Re-run the install command. One thing to know: **end your sessions first.**

Installing replaces the binary on disk, and a running session bind-mounts that same
binary into its sandbox as the lockdown shim. Once it is replaced, the worker's own
`/proc/self/exe` reads `".../cowboy (deleted)"`. Cowboy resolves that — the replacement
is at the same path — so the session keeps working. But if the binary moved somewhere
else entirely, the session cannot confine anything and refuses to run commands rather
than running them unconfined, telling you to restart it. The daemon is unaffected:
`cowboy down` and starting again picks up the new version.

The web UI is embedded in the binary, so a running `cowboyd` keeps serving the version
it started with. `cowboy web off && cowboy web on` after an upgrade, or let the daemon
idle out.

## The toolchain the agent gets

Yours. `/usr` and `/opt` are exposed read-only inside the sandbox, so the agent
runs the compilers, language runtimes and CLIs you have installed, at the versions
you installed — nothing to build, pull, or keep in sync with an image.

If a project needs a toolchain you do not have on the host, install it on the host
(or let the project's own `mise`/`asdf` setup do it in the workspace, which is
writable).

## Developing cowboy

```sh
git clone https://github.com/koshea/cowboy && cd cowboy
cargo install --locked --path crates/cowboy-cli
```

See [Contributing](../contributing.md) for the test suite, including the sandbox
integration tests and how to stop them skipping silently.

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
cowboy doctor                   # kernel prerequisites, model config, daemon
cowboy sandbox plan             # exactly what the next command will be able to see
```

`cowboy sandbox plan` is worth running once before you start: it prints the real
bind list, what is masked, the kernel-level lockdown, and the paths that can never
be granted — so the boundary is something you read rather than something you trust.
