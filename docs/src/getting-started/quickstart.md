# Quick start

```sh
cd your-project
cowboy init                 # writes .cowboy/{security,agent,models}.yaml
cowboy doctor               # check Docker, Linux, nft, model, Compose
cowboy "run the tests and fix one simple failure"
```

## Interactive mode is conversational

On a terminal, `cowboy` (no task, or with a seed task) is a persistent
conversational REPL — like Claude Code. The agent answers a turn, then returns to
the prompt for your next message, keeping the **full conversation and the same
container** alive. The `final` tool ends a *turn*, not the session.

- **Ctrl-C** opens an interrupt menu: `k` cancels the current turn (you keep
  going), `e` ends the session (finalizes the log).
- **Piped / non-TTY** runs (`cowboy "task" | …`) stay single-shot, for scripting.

The default network policy allows common dev registries (npm, PyPI, Go, crates,
RubyGems, Debian, GitHub) so `npm`/`pip`/`go`/`cargo`/`apt` installs work out of
the box, including non-interactively.

## Common commands

```
cowboy                       # interactive conversational TUI (multi-turn)
cowboy "fix the tests"       # seed the conversation (TTY) / one-shot (piped)
cowboy init [--git]          # write .cowboy/{security,agent,models}.yaml
cowboy doctor                # environment checks
cowboy run <cmd>             # run a command in the agent container
cowboy shell                 # interactive shell in the agent container
cowboy patch show|save|apply|check|revert
cowboy proc list|start|stop|restart|logs <name>
cowboy skill list|show <name>
cowboy sessions              # list live/registered sessions
cowboy logs                  # list past sessions
cowboy replay <id>           # replay a past session
cowboy down [--all]          # stop/remove this project's (or all) containers + networks
```

See the [CLI reference](../reference/cli.md) for the full, auto-generated command
tree, and [Ranch Plans](../ranch/overview.md) for multi-workstream orchestration.
