# Contributing to Cowboy

Thanks for your interest in contributing! Cowboy is MIT-licensed (see
[LICENSE](LICENSE)).

## Start here

- **[AGENTS.md](AGENTS.md)** is the authoritative developer guide — workspace
  layout, conventions, the security invariants you must preserve, testing, and
  gotchas. A `CLAUDE.md` defers to it.
- The [documentation site](docs/src/SUMMARY.md) (mdBook under `docs/`) covers the
  product and architecture; its [Contributing](docs/src/contributing.md) chapter
  mirrors this page.

## Before you open a PR

```sh
cargo fmt --all                              # format
cargo clippy --workspace --all-targets       # must be clean
cargo nextest run                            # or: cargo test --workspace
cargo test --doc
```

- **Keep the docs current.** If you add or change a feature, update its chapter in
  `docs/src/` in the same change. The CLI reference is auto-generated — after a CLI
  change run `COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs`. CI
  fails if `docs/src/reference/cli.md` is stale or the book doesn't build.
- **Preserve the security boundary.** The agent is never part of it. Don't move
  enforcement into the prompt, and don't expose host-owned config or credentials.
  See [SECURITY.md](SECURITY.md) and the
  [security model](docs/src/security/model.md).
- **Model-dependent behavior** is covered by `#[ignore]` end-to-end tests; run the
  relevant ones (`cargo test -p cowboy-cli --test daemon_e2e -- --ignored`) and say
  whether they passed.

## Reporting bugs & vulnerabilities

- Functional bugs: open a GitHub issue with steps to reproduce.
- **Security issues: do not open a public issue** — see [SECURITY.md](SECURITY.md).

## Licensing of contributions

Unless you state otherwise, contributions you submit are licensed under the
project's MIT license.
