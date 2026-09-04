# AGENTS.md — working in the `cowboy` repo

Guidance for AI coding agents (and humans) working on **cowboy** itself. For the
product overview and the security rationale, read `README.md` and the docs site
(`docs/src/`, an mdBook).

## What this is

`cowboy` (binary `cowboy`, daemon `cowboyd`) is an opinionated local coding agent
that runs the AI in a **host-native sandbox** — Linux namespaces, Landlock,
seccomp, an empty capability set — while the **host** enforces security at the
kernel + network layer.

**The one inviolable principle:** the agent is **not** part of the security
boundary. Security is enforced by the kernel, host-owned config, and a host-side
policy engine — *never* by prompting the model. When you change anything near
config, mounts, networking, credentials, or the agent loop, preserve this. See
`docs/src/security/model.md`, and `docs/src/security/sandbox-decisions.md` for the
evidence behind each mechanism (read it before redesigning any of them — most of
the obvious alternatives were tried and rejected for recorded reasons).

## Build / test / run

```sh
cargo build                                  # whole workspace
cargo nextest run                            # unit + integration
cargo test --doc                             # doctests (nextest skips these)
cargo test --workspace                       # works too if you don't have nextest
cargo clippy --workspace --all-targets       # must be clean (no custom lint config; defaults)
cargo fmt --all                              # rustfmt defaults; CI-style check: `--all -- --check`

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
- **Sandbox suites** (`tests/sandbox_exec.rs`, `sandbox_session.rs`,
  `sandbox_egress.rs`) exercise real namespaces, Landlock, nftables and the relay.
  They **self-skip** when the host cannot run them, so a broken probe can make a
  whole file pass while doing nothing — that has happened. Always verify with:
  ```sh
  COWBOY_SANDBOX_TESTS=required cargo nextest run -p cowboy-cli \
      --test sandbox_exec --test sandbox_session --test sandbox_egress
  ```
  `required` turns a skip into a failure. Two traps: **a successful `connect()` is
  not evidence of egress** (under transparent interception every connect succeeds —
  attempt a data transfer), and **denial tests pass vacuously offline** (hence a
  separate `skip_if_offline!()`).

  Host-capability *unit* tests (`doctor::this_host_reports_no_sandbox_failures`,
  `preflight::the_host_meets_every_requirement`) honour the same switch. They used to
  assert unconditionally, which failed on any host that cannot sandbox — including CI.
- **Cgroup tests** have their own switch, `COWBOY_CGROUP_TESTS=required`. Resource
  limits are deliberately **not** part of the boundary, so `COWBOY_SANDBOX_TESTS`
  must not demand them: a CI runner has no delegated subtree and should still be able
  to require a working sandbox. On a systemd user session, set both.
- **`#[ignore]` end-to-end tests** are the **manually-run suite** — they spawn real
  worker processes and need a real model provider. They self-skip when
  prerequisites are absent, so `--ignored` is safe to run anywhere:
  ```sh
  cargo test -p cowboy-cli --test daemon_e2e -- --ignored
  ```
  This is the regression net for model-dependent behavior (prompts, tool use,
  Ranch coordination) — keep adding to it as features land, and **always clean up**
  (end the worker / remove the worktree / let the session's namespaces be reaped).

## Workspace & where things live

```
crates/
  cowboy-cli/      the `cowboy`/`cowboyd` binaries
    src/cli.rs       clap command tree            src/main.rs  dispatch
    src/cmd/         one module per CLI command (daemon.rs, worker.rs, ranch.rs, session.rs, web.rs, …)
    src/agent/       the agent loop (run.rs), tool defs (tools.rs), UI impls (ui.rs/tui.rs/socket_ui.rs)
    src/sandbox/     THE BOUNDARY: session.rs (namespaces + holder), native.rs (the Sandbox impl),
                     bwrap.rs, exec.rs, shim.rs, lockdown.rs (Landlock+seccomp+caps), cgroup.rs,
                     grants.rs, preflight.rs, policy.rs, transport/ (nft.rs, relay.rs, broker.rs,
                     channel.rs = the enforcement boundary)
    src/net/         persisted network approvals, git worktrees
    src/project.rs   project identity + host-side helpers (hash, repo key, private files)
    src/session/     session logging / replay
  cowboy-core/     shared types, pure logic, + the model transport
    config.rs model.rs policy.rs ranch.rs scope.rs artifact.rs
    lifecycle.rs decision.rs memory.rs tokens.rs usersecrets.rs error.rs
  cowboy-proto/    wire types (daemonproto, netproto) — serde-only, also compiles to wasm
  cowboy-tui/      ratatui rendering (snapshot-tested)
  cowboy-sandbox/  the sandbox plan as PURE LOGIC (binds, Landlock rules, seccomp, denylist)
  cowboy-gateway/  the policy engine as a LIBRARY (policy, DNS, ip->domain attribution)
  cowboy-web-ui/   Yew/WASM remote-control frontend (NOT a workspace member —
                   wasm32-only; `trunk build` in this dir, embedded by cowboy-cli.
                   cowboy-cli/build.rs embeds an existing dist/, or builds one
                   when COWBOY_WEB_UI=1; plain dev builds skip it → placeholder)
docs/
```

Rough split: **`cowboy-core`** = data types + pure logic (serde structs, policy,
the wire protocols) **plus the model transport** — the `ModelClient` trait and
the streaming OpenAI-compatible `OpenAiClient` live here (`model.rs`), since the
HTTP/SSE client is shared and has no CLI/daemon dependencies. **`cowboy-sandbox`**
= the plan as pure logic, so what the boundary *is* can be unit-tested and
snapshotted without creating a namespace. **`cowboy-cli`** = everything else: the
sandbox executor, the daemon, the agent loop, the CLI. The **agent loop runs
host-side** in the worker process; the sandbox confines the agent's *shell
commands*, not the loop.

## Conventions

- **Serialization:** `serde_yaml_ng` for YAML config/plans; `serde_json` for jsonl
  logs and wire messages. `daemonproto` (`DaemonReq`/`Resp`) is internally tagged
  on `kind` (snake_case); `ServerMsg`/`ClientMsg`/`UiEventMsg` are externally
  tagged snake_case. Avoid internally-tagged enums with newtype-string variants
  (they break serde here) — use struct variants or external tagging.
- **Config:** the `security.yaml` section is `sandbox:` (was `container:`). A config
  still using the old key is **refused with a clear error**, not silently ignored —
  ignoring it would drop every mount under it without saying so. In non-security
  config (`agent.yaml`) a serde `alias` is the friendlier choice, since a silent
  default there costs a timeout rather than a boundary.
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
  consumed host-side; never written into a project or bound into the sandbox.
- Host-owned `security.yaml` / `models.yaml` are masked inside the sandbox (the
  mask bind is **last** so nothing re-exposes it); `SecurityConfig::validate`
  refuses mounts that expose `.cowboy`/`security.yaml`.
- **Containment must not depend on the nftables ruleset.** The session netns holds
  no host-connected device, so a transport that fails to install leaves *no* egress
  rather than open egress. If you change the transport, preserve that inversion —
  it is the property that justified the rewrite.
- **The relay↔engine channel is the enforcement boundary** (`transport/channel.rs`).
  It is an anonymous `socketpair` with no filesystem or abstract name. Never give it
  a name, and never let the relay's *report* of a destination be the only thing
  standing between the agent and egress in a new code path.
- The relay **never creates an outbound socket** — the engine dials in the host
  netns and passes the connected fd. So there is no uid exemption in the ruleset,
  and there must never be one: the agent is uid 0 in its own user namespace, so
  `skuid 0` would exempt the *agent*.
- **Landlock is a hard requirement, not best-effort**, and its rules use
  sandbox-internal (bind *target*) paths. A missing rule path is a hard error — an
  earlier `filter_map(ok)` silently produced a zero-rule domain that looked like
  working confinement.
- **Runtime grants and network approvals are stored host-side**
  (`~/.config/cowboy/{grants,approvals}/`), never in the workspace — it is writable
  from inside, so a file there would let a hostile repo widen its own access. The
  credential denylist is re-checked **at use**, not only at write.
- Default policy `ask` **fails closed** with no approver. Never substitute
  prompting for enforcement.
- Resource limits (cgroup v2) are **not** part of the boundary. Keep that
  distinction: `doctor` warns for them and fails for the rest.

## Ranch Plans (multi-workstream orchestration)

A large task split into dependency-aware workstreams, each a normal Cowboy session
in its own worktree/branch. `cowboy-core/src/ranch.rs` (data model + readiness) +
`scope.rs` (proposals); `cmd/ranch.rs` (CLI + advance/promote logic);
the **coordinator** in `cmd/daemon.rs` auto-advances on workstream completion.

Invariants to preserve here:
- `.cowboy/ranches/<id>/ranch.yaml` is the **committed source of truth**. Its
  **scope** — which workstreams exist, their goals, `depends_on`, expected artifacts
  and acceptance criteria — is **never** changed by an agent, and never autonomously
  by the coordinator: scope changes go through `propose_scope_change` →
  `cowboy ranch approve` (user-gated). Its **progress** — statuses, `session_id`,
  `branch`, `worktree_path`, timestamps — is written all day by the coordinator and by
  `ranch start/complete/accept/retry`. That split is enforced, not just documented:
  progress paths call `ranch::save_progress(root, before, after)`, which refuses the
  write if `Ranch::scope_fingerprint()` changed. Use plain `ranch::save` only on a
  user-gated scope path.
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
  and kills it. Use `pkill -x cowboyd` / `pgrep -x cowboyd`. Same trap applies to
  `pgrep -f` in tests: count processes by reading `/proc` instead.
- Per-project teardown: `cowboy down`.
- The daemon persists state to `$XDG_STATE_HOME/cowboy/daemon/state.json`; sockets
  live under `$XDG_RUNTIME_DIR/cowboy`.
- **Linux only**, and currently targeted at one host: a current kernel with
  Landlock ABI 6+, unprivileged user namespaces, and a delegated cgroup v2 subtree.
  `cowboy doctor` checks each by performing it. Porting to other distributions is a
  deliberate follow-up — see the notes in `docs/src/security/sandbox-decisions.md`.
- `--die-with-parent` is **load-bearing** for reaping bwrap's process tree, and
  `--remount-ro /` must stay the **last** mount operation. Special filesystems
  (`--proc`/`--dev`/`--tmpfs`) come **before** the binds, or a tmpfs shadows grants
  under `/tmp`.

## Before you commit

`cargo fmt --all` · `cargo clippy --workspace --all-targets` (clean) ·
`cargo nextest run` (or `cargo test --workspace`). If you touched a snapshotted
surface, review the `insta` diff. If you changed the CLI, regenerate `cli.md`
(above). If you added/changed a feature, update its docs chapter. If you changed
model-dependent behavior, run the relevant `#[ignore]` E2E and report whether it
passed.

**If you touched `cowboy-proto` (or anything else the web UI consumes), build the web
UI too** — it is wasm32-only and **not a workspace member**, so `--workspace` never
compiles it and a break is invisible to every command above:

```sh
cd crates/cowboy-web-ui && cargo test && trunk build --release
```

This has bitten once already: adding `UiEventMsg::ContextUsage` for the TUI's
`/context` view left the web UI's `match` non-exhaustive, and nothing local failed —
`dist/` kept embedding a stale bundle, so even `COWBOY_WEB_UI_TESTS=required` passed
(it asserts a bundle *is* embedded, not that one *builds*). CI has a dedicated job that
would have caught it; that is no help before you push. Note also that
`cargo install --path` re-resolves dependencies and ignores `Cargo.lock` — always
`--locked`.
