# Contributing

The authoritative contributor guide is **`AGENTS.md`** at the repo root (a
`CLAUDE.md` defers to it so Claude Code picks it up automatically). It covers the
workspace layout, conventions, the host-handled-tool pattern, security invariants,
and gotchas. This page summarizes the essentials and the **docs workflow**.

## Build, test, lint

```sh
cargo build
cargo nextest run                          # unit + integration
cargo test --doc
cargo clippy --workspace --all-targets     # must be clean
cargo fmt --all                            # rustfmt defaults
```

### The sandbox suites, and how to stop them lying

`tests/sandbox_exec.rs`, `sandbox_session.rs` and `sandbox_egress.rs` exercise real
namespaces, real Landlock, real nftables and a real relay. They **self-skip** when
the host cannot run them — which means a broken probe can make the whole file pass
while doing nothing. That happened during development, so there is a guard:

```sh
COWBOY_SANDBOX_TESTS=required cargo nextest run -p cowboy-cli \
    --test sandbox_exec --test sandbox_session --test sandbox_egress
```

`required` turns a skip into a failure. Always verify with it. The wall-clock tell
is obvious once you know it: a few seconds when they run, a few milliseconds when
they skip.

**Run both `cargo nextest run` and `cargo test`.** They are not interchangeable here.
nextest gives each test its own process, so anything keyed on the pid — the sandbox
scratch directory is — is unshared, and a race between two tests cannot occur.
`cargo test` runs a binary's tests as threads in one process, which is what CI does. A
TOCTOU in the config-mask creation survived every nextest sweep for exactly that reason
and failed on the first CI run that got far enough to execute the tests.

The same switch governs the host-capability *unit* tests (`cowboy doctor`'s own
check, and the preflight's). Those used to assert unconditionally, so they failed on
any machine that cannot sandbox — a CI runner without bubblewrap, or Ubuntu 24.04,
which gates unprivileged user namespaces behind AppArmor
(`kernel.apparmor_restrict_unprivileged_userns=1`; installing bwrap is not enough).

**Cgroup tests use a separate switch,** `COWBOY_CGROUP_TESTS=required`. Resource
limits are deliberately not part of the security boundary, so `COWBOY_SANDBOX_TESTS`
does not demand them — a runner with no delegated cgroup subtree should still be able
to require a working sandbox. On a systemd user session, set both.

Two traps worth knowing before you write a test here:

- **A successful `connect()` is not evidence of egress.** Under transparent
  interception *every* `connect()` succeeds — it reaches the relay on loopback, and
  policy has not been consulted yet. Attempt a data transfer; a refusal appears as a
  connection reset on the first read.
- **Denial tests pass vacuously when the network is down.** `skip_if_offline!()` is
  separate from `skip_if_unsupported!()` for exactly that reason.

The `#[ignore]` end-to-end tests are the **manually-run suite** for model-dependent
behavior (run with `cargo test -p cowboy-cli --test daemon_e2e -- --ignored`).
Always clean up the worktrees they create.

## The web UI is invisible to `--workspace`

`cowboy-web-ui` is wasm32-only and deliberately **not a workspace member**, so
`cargo build --workspace`, `clippy --workspace` and `nextest run` never compile it. If
you change `cowboy-proto` — or anything else it consumes — build it yourself:

```sh
cd crates/cowboy-web-ui && cargo test && trunk build --release
```

This is not hypothetical. Adding a `UiEventMsg` variant for the TUI's `/context` view
left the web UI's `match` non-exhaustive and **nothing local failed**: `dist/` still
held a bundle from before the change, so `cowboy-cli` embedded it happily and even
`COWBOY_WEB_UI_TESTS=required` passed — it asserts a bundle *is* embedded, not that one
still *builds*. CI has a dedicated job with `trunk` that catches this, which is no help
until you push.

Two habits follow from it: keep `apply_event`'s match exhaustive (no `_ =>` arm, so the
compiler is the guard), and rebuild `dist/` before installing, or you ship a binary
whose embedded UI predates the protocol it speaks.

Also: `cargo install --path` **re-resolves dependencies and ignores `Cargo.lock`**. Use
`--locked`, as the install docs do, or you build against versions nobody has tested.

## Keeping these docs up to date

This site is part of the change, not an afterthought. **When you add or change a
feature, update the docs in the same change.**

- **Find the right chapter** under `docs/src/` (or add one and link it in
  `docs/src/SUMMARY.md`). The chapter map mirrors the feature areas: getting
  started, security, using Cowboy, Ranch Plans, reference.
- **The CLI reference is auto-generated.** `docs/src/reference/cli.md` is produced
  from the clap command tree. If you change the CLI, regenerate it:

  ```sh
  COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs
  ```

  A normal test run (`cargo test`) **fails** if `cli.md` is stale, so the
  reference can't silently drift from the code.
- **Build the book** to catch broken links / missing `SUMMARY.md` entries:

  ```sh
  mdbook build docs        # serve live with: mdbook serve docs
  ```

  (Install once with `cargo install mdbook`.) A test also runs `mdbook build` when
  `mdbook` is on `PATH`, skipping otherwise — so CI with `mdbook` installed guards
  the book, and local runs without it don't fail.

## Conventions in one breath

`serde_yaml_ng` for YAML, `serde_json` for jsonl; `u64` `now_ms()` timestamps (no
`chrono`); user-facing output goes through the `AgentUi` trait; inject side effects
as closures to keep logic unit-testable; match the surrounding code's terse,
why-focused comments. See `AGENTS.md` for the details.
