# cowboy examples

Each directory is a minimal project you can run `cowboy` in:

```sh
cd examples/<name>
cowboy init          # writes .cowboy/{security,agent}.yaml
cowboy models setup  # once per machine: endpoint + key, stored host-side
cowboy doctor
cowboy "..."         # give the agent a task
```

- **basic/** — an empty project; smallest possible starting point.
- **rust/** — a tiny Rust crate (the agent can `cargo build`/`cargo test`), with a
  project skill under `.cowboy/skills/`.
- **node/** — a tiny Node project (`npm`/`pnpm`).
