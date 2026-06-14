# cowboy examples

Each directory is a minimal project you can run `cowboy` in:

```sh
cd examples/<name>
export COWBOY_OPENAI_API_KEY=...
cowboy init        # writes .cowboy/{security,agent,models}.yaml
cowboy doctor
cowboy "..."       # give the agent a task
```

- **basic/** — an empty project; smallest possible starting point.
- **rust/** — a tiny Rust crate (the agent can `cargo build`/`cargo test`).
- **node/** — a tiny Node project (`npm`/`pnpm`).
- **compose/** — a Docker Compose project; `cowboy init`/`doctor` will offer to
  approve the agent joining its network (see [docs/NETWORK.md](../docs/NETWORK.md)).
