# AGENTS.md — working in the `cowboy` repo

Guidance for AI coding agents (and humans) working on **cowboy** itself. For the
product overview and the security rationale, read `README.md` and the docs site
(`docs/src/`, an mdBook).

## What this is

`cowboy` (binary `cowboy`, daemon `cowboyd`) is an opinionated local coding agent
that runs the AI inside a Docker-contained dev environment while the **host**
enforces security at the container + network layer.

**The one inviolable principle:** the agent is **not** part of the security
boundary. Security is enforced by Docker, host-owned config, and a Cowboy-owned
network gateway — *never* by prompting the model. When you change anything near
config, mounts, networking, credentials, or the agent loop, preserve this. See
`docs/src/security/model.md`.

## Build / test / run

```sh
cargo build                                  # whole workspace
cargo nextest run                            # unit + integration (Docker E2E auto-skips if absent)
cargo test --doc                             # doctests (nextest skips these)
cargo test --workspace                       # works too if you don't have nextest
cargo clippy --workspace --all-targets       # must be clean (no custom lint config; defaults)
cargo fmt --all                              # rustfmt defaults; CI-style check: `--all -- --check`

docker/build.sh                              # build agent + gateway images (or `… agent|gateway`)
```

Run a one-off task locally: `cowboy "do X"` (in a project with `.cowboy/`). The
daemon path: `cowboyd` supervises worker processes; a TUI/headless client attaches
over a unix socket.

### Tests

- **Unit tests** live beside code (`#[cfg(test)] mod tests`). Pure logic is made
  unit-testable by **injecting** side effects as closures (see
  `cmd/ranch.rs::reconcile_and_pick`) rather than reaching for a daemon/disk.
- **Snapshot tests** use `insta` (e.g. the agent tool surface, TUI rendering).
  Update intentionally: `INSTA_UPDATE=always cargo test …`, then review the diff.
- **`#[ignore]` end-to-end tests** are the **manually-run suite** — they spawn real
  worker processes, and some need Docker + a real model provider. They self-skip
  when prerequisites are absent, so `--ignored` is safe to run anywhere:
  ```sh
  cargo test -p cowboy-cli --test daemon_e2e -- --ignored
  ```
  This is the regression net for model-dependent behavior (prompts, tool use,
  Ranch coordination) — keep adding to it as features land, and **always clean up**
  (`reap_new_docker` helper / end the worker / remove the worktree).

## Workspace & where things live

```
crates/
  cowboy-cli/      the `cowboy`/`cowboyd` binaries
    src/cli.rs       clap command tree            src/main.rs  dispatch
    src/cmd/         one module per CLI command (daemon.rs, worker.rs, ranch.rs, session.rs, …)
    src/agent/       the agent loop (run.rs), tool defs (tools.rs), UI impls (ui.rs/tui.rs/socket_ui.rs)
    src/net/         docker (docker.rs), runtime spec (runtime.rs), gateway, worktree, control socket
    src/session/     session logging / replay
  cowboy-core/     shared types + logic (no I/O orchestration)
    config.rs daemonproto.rs model.rs policy.rs ranch.rs scope.rs artifact.rs
    lifecycle.rs decision.rs memory.rs tokens.rs usersecrets.rs error.rs
  cowboy-tui/      ratatui rendering (snapshot-tested)
  cowboy-gateway/  the sole-egress gateway binary (proxy + DNS + nft policy)
docker/  docs/
```

Rough split: **`cowboy-core`** = data types + pure logic (serde structs, policy,
the wire protocols). **`cowboy-cli`** = orchestration (Docker, the daemon, the
agent loop, CLI). The **agent loop runs host-side** in the worker process; the
Docker container is the sandbox for the agent's *shell commands*, not for the loop.

## Conventions

- **Serialization:** `serde_yaml_ng` for YAML config/plans; `serde_json` for jsonl
  logs and wire messages. `daemonproto` (`DaemonReq`/`Resp`) is internally tagged
  on `kind` (snake_case); `ServerMsg`/`ClientMsg`/`UiEventMsg` are externally
  tagged snake_case. Avoid internally-tagged enums with newtype-string variants
  (they break serde here) — use struct variants or external tagging.
- **Timestamps:** `u64` milliseconds since epoch via a local `now_ms()`. **No
  `chrono`.**
- **Errors:** `anyhow::Result` in `cowboy-cli`; `cowboy_core::error::{Error,Result}`
  in `cowboy-core`.
- **Adding a host-handled agent tool** (the `memory`/`blocked`/`artifact` pattern):
  1. `TOOL_*` const + an `…Args` struct (derive `Deserialize, JsonSchema`) in
     `agent/tools.rs`; 2. a `ToolDef` in `definitions()`; 3. a dispatch arm in
     `AgentLoop::handle_tool_calls` (`agent/run.rs`); 4. a `run_*` handler.
  This changes the tool-surface snapshot and the `definitions_cover_the_tool_surface`
  list — update both.
- **UI:** anything user-facing goes through the `AgentUi` trait (`agent/ui.rs`),
  impl'd by `ConsoleUi`, `TuiUi`, `SocketUi`, and `RecordingUi` (tests) — don't
  `println!` from the loop.
- **Match the surrounding code** (terse doc comments explaining *why*, not what).

## Security invariants — do not break

- Provider credentials live only in `~/.config/cowboy/providers.yaml` (`0600`),
  consumed host-side; never written into a project or mounted.
- Host-owned `security.yaml` / `models.yaml` are masked inside the container;
  `SecurityConfig::validate` refuses mounts that expose `.cowboy`/`security.yaml`.
- Network egress is route-enforced through the gateway; default policy `ask`
  **fails closed** with no approver. Never substitute prompting for enforcement.
- Don't widen the boundary silently — `privileged`/`docker_socket` are surfaced by
  `cowboy doctor` as warnings on purpose.

## Ranch Plans (multi-workstream orchestration)

A large task split into dependency-aware workstreams, each a normal Cowboy session
in its own worktree/branch. `cowboy-core/src/ranch.rs` (data model + readiness) +
`scope.rs` (proposals); `cmd/ranch.rs` (CLI + advance/promote logic);
the **coordinator** in `cmd/daemon.rs` auto-advances on workstream completion.

Invariants to preserve here:
- `.cowboy/ranches/<id>/ranch.yaml` is the **committed source of truth** and is
  **never edited by an agent** (nor autonomously by the coordinator). Scope changes
  go through `propose_scope_change` → `cowboy ranch approve` (user-gated).
- Coordination is **artifact-driven**, not chat: workstreams publish artifacts /
  handoffs that get promoted into the ranch store for downstream consumers.
- **Acceptance gates** pause a finished workstream for human sign-off
  (`cowboy ranch accept`) rather than auto-completing.

## Documentation — keep it current

The docs site is an **mdBook** at `docs/` (`docs/book.toml`, content in
`docs/src/`, TOC in `docs/src/SUMMARY.md`). **Docs are part of the change, not an
afterthought — when you add or change a feature, update the relevant chapter in the
same change.** The chapter map mirrors the feature areas (getting started,
security, using Cowboy, Ranch Plans, reference).

Two guards keep it honest (both run under `cargo test`):

- **CLI reference is auto-generated** from the clap tree into
  `docs/src/reference/cli.md`. After any CLI change, regenerate:
  `COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs`. A normal test run
  **fails** if it's stale — never hand-edit `cli.md`.
- **The book must build:** the `book_builds` test runs `mdbook build docs` when
  `mdbook` is on PATH (skips otherwise), catching broken links / missing
  `SUMMARY.md` entries. Install once with `cargo install mdbook`; preview with
  `mdbook serve docs`.

## Gotchas

- **Never `pkill -f cowboyd`** — the pattern matches the shell running the command
  and kills it. Use `pkill -x cowboyd` / `pgrep -x cowboyd`.
- Per-project teardown: `cowboy down`. Reap stray containers:
  `docker rm -f $(docker ps -aq --filter label=cowboy=1)`.
- The daemon persists state to `$XDG_STATE_HOME/cowboy/daemon/state.json`; sockets
  live under `$XDG_RUNTIME_DIR/cowboy`.
- Linux + Docker + `nftables` are required for the full stack (`cowboy doctor`
  checks them).

## Before you commit

`cargo fmt --all` · `cargo clippy --workspace --all-targets` (clean) ·
`cargo nextest run` (or `cargo test --workspace`). If you touched a snapshotted
surface, review the `insta` diff. If you changed the CLI, regenerate `cli.md`
(above). If you added/changed a feature, update its docs chapter. If you changed
model-dependent behavior, run the relevant `#[ignore]` E2E and report whether it
passed.
