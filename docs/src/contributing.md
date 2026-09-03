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
