# Your first five minutes

A walkthrough of one real session, start to finish, so you know what to expect
before you hand an agent your repository. [Quick start](quickstart.md) is the
command cheat-sheet to come back to; this is the path through it once.

Assumes `cowboy` and `cowboyd` are [installed](installation.md).

## 1. Point it at a project

```sh
cd your-project
cowboy
```

If anything is missing, that is what you get — all of it, in the order to fix it,
rather than one error at a time:

```text
cowboy can't start in /home/you/your-project
  ✗ no .cowboy/ config in /home/you/your-project  →  cowboy init
  ✗ no model provider configured                  →  cowboy models setup

run them in that order, then `cowboy` again
```

You can run `cowboy` from any subdirectory — it finds the project by walking up for
`.cowboy/`, then for `.git/`. The directory it settles on is the one the agent gets;
nothing above it is visible.

## 2. `cowboy init`

```text
  created /home/you/your-project/.cowboy/security.yaml
  created /home/you/your-project/.cowboy/agent.yaml
  updated /home/you/your-project/.gitignore

Initialized cowboy config in /home/you/your-project/.cowboy
  - security.yaml (host-owned, never mounted)
  - agent.yaml (read by the agent loop)

Next: run `cowboy models setup` to configure a model provider, then `cowboy doctor`.
```

Two files, and they have different jobs. `security.yaml` is the **boundary** — what
the agent may read, write and reach. It is host-owned and masked inside the sandbox,
so the agent cannot read it, let alone edit it. `agent.yaml` is behaviour: timeouts,
iteration caps, the things a wrong value costs you a retry rather than a breach.
Commit both; `security.yaml` is a reviewable artifact of what you allowed.

## 3. `cowboy models setup`

Cowboy talks to one OpenAI-compatible endpoint and ships no keys. This asks for an
endpoint and a key, then a model that uses it. Both steps are required — a provider
with no model leaves you with an error at the first turn instead of at setup.

Credentials go to `~/.config/cowboy/providers.yaml` (mode `0600`), read host-side.
They are never written into your project and never bound into the sandbox, so the
agent cannot exfiltrate a key it has no way to read.

## 4. `cowboy doctor`

Every check is performed, not assumed:

```text
[ ok ] bubblewrap             /usr/bin/bwrap (not setuid)
[ ok ] user namespaces        unprivileged namespaces work
[ ok ] landlock               ABI 10 (>= 6)
[ ok ] seccomp                filtering available (kill_process kill_thread …)
…
configuration
[ ok ] security.yaml          v1, policy=Ask
[ ok ] agent.yaml             timeout=600s, max_iter=100
[ ok ] config separation      security.yaml is host-only (masked, never mounted)
```

Anything that fails ends with a verdict telling you which kind of problem you have,
because the two need different responses:

- `cowboy cannot start here yet — start with …` is configuration. Fixable now.
- `the sandbox cannot run on this host (…)` is the kernel. Cowboy will refuse to run
  commands rather than run them unconfined.

Resource limits (cgroups) only ever **warn**: they are not part of the boundary, so a
host without a delegated cgroup subtree can still confine an agent properly.

## 5. Give it something small

```sh
cowboy "run the tests and fix one simple failure"
```

The TUI opens and streams the work: commands as they run, file edits as inline
diffs, and the network pane on the right. **F1** lists every key and slash command;
that is the only thing worth memorising.

Three things to try while it works, because none of them are guessable:

- **Type.** Your message reaches the agent at its next step, so "also check the error
  path" lands mid-turn instead of after it. You do not need to interrupt to be heard.
- **Ctrl-C.** Interrupts, immediately — no menu. It stops the turn and puts the cursor
  in the input so you can say what to do instead; the conversation is kept.
- **`/diff`.** Shows the working tree. The workspace is bind-mounted, so the agent's
  edits are already in your real files.

## 6. When it asks for the network

Egress is denied by default, so the first time the agent reaches somewhere new you
get a prompt:

```text
╭ Network request ─────────────────────────────────────────────╮
│crates.io:443                                                 │
│                                                              │
│requested by:  cargo test --workspace                         │
│                                                              │
│o  once — just this request                                   │
│s  session — every request here until this session ends       │
│p  project — always allow here (saved for this repo)          │
│g  global — always allow everywhere                           │
│d  deny                                                       │
╰──────────────────────────────────────────────────────────────╯
```

It names the command that wants the destination, not just the destination — that is
usually what decides it. An approval grants **exactly** what the prompt showed: that
host, that port. Nothing broader. Project and global choices are saved host-side, so a
repository cannot widen its own access by writing a file.

Common dev registries (npm, PyPI, crates.io, Go, RubyGems, Debian, GitHub) are
allowed by the default policy, so package installs work without any of this.

## 7. Keep the work

The agent edited your real working tree, so commit with plain `git` — `git add -p`,
`git commit` — or ask the agent to do it. `cowboy patch show` prints the diff and
`cowboy patch save` writes it to `.cowboy/diff.patch`.

To step away: **Alt-d** detaches and leaves the session running under the daemon.
`cowboy sessions` lists what is live and `cowboy attach <id>` rejoins. To finish,
`/quit` — or Ctrl-C twice at an empty prompt.

## What to read next

- [How-to guides](../how-to.md) — steering, interrupting, worktrees, the web UI.
- [The boundary](../security/model.md) — what actually confines the agent, and why
  the agent is deliberately not part of it.
- [The crew](../using/crew.md) — routing delegated work to cheaper or stronger
  models, once one model doing everything starts to chafe.
