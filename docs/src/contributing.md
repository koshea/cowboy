# Contributing

The authoritative contributor guide is **`AGENTS.md`** at the repo root (a
`CLAUDE.md` defers to it so Claude Code picks it up automatically). It covers the
workspace layout, conventions, the host-handled-tool pattern, security invariants,
and gotchas. This page summarizes the essentials and the **docs workflow**.

## Build, test, lint

```sh
cargo build
cargo nextest run                          # unit + integration (Docker E2E auto-skips if absent)
cargo test --doc
cargo clippy --workspace --all-targets     # must be clean
cargo fmt --all                            # rustfmt defaults
docker/build.sh                            # build the agent + gateway images from source
```

A source checkout builds the agent/gateway images from your tree automatically
(installed-from-git binaries pull from GHCR instead); `docker/build.sh` just does
it eagerly. Set `COWBOY_SRC` to force source builds from any binary.

The `#[ignore]` end-to-end tests are the **manually-run suite** for model-dependent
behavior (run with `cargo test -p cowboy-cli --test daemon_e2e -- --ignored`).
Always clean up containers/worktrees they create.

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
