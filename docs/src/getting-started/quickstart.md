# Quick start

The commands, condensed. For a narrated walkthrough of one session — what each step
prints and what to do when the agent asks for the network — see
[Your first five minutes](first-five-minutes.md).

```sh
cd your-project
cowboy init                 # writes .cowboy/{security,agent}.yaml
cowboy models setup         # a provider + model (stored host-side); more: `cowboy providers add`
cowboy doctor               # kernel prerequisites, model config, daemon
cowboy "run the tests and fix one simple failure"
```

## Quick start with a ChatGPT subscription

No API key needed: a ChatGPT plan pays for the OpenAI models, and Cowboy signs in
with your account.

```sh
cowboy providers add chatgpt --chatgpt   # opens the browser: "Continue with ChatGPT"
                                         #   then: import your models? → yes, [a]ll
                                         #   then: add an `openai` crew? → yes
cd your-project
cowboy init
cowboy doctor                            # also checks the sign-in
cowboy "run the tests and fix one simple failure"
```

The three answers do the setup:

1. **The sign-in** creates the `chatgpt` provider (`api: responses`, `auth:
   chatgpt`) and keeps its OAuth tokens in `~/.config/cowboy/auth/chatgpt.yaml`
   (`0600`), host-side, where the agent cannot reach them. On a remote machine,
   forward the callback port first: `ssh -L 1455:127.0.0.1:1455 <host>`.
2. **The import** saves the models your account offers as `chatgpt/<model>`, each
   with its context window and the reasoning levels it accepts. The first one becomes
   your default.
3. **The `openai` crew** is the [built-in ChatGPT crew](../how-to.md#the-built-in-chatgpt-crew).
   It becomes the active crew when it is your first. The session runs on its foreman,
   your newest sol model (today `chatgpt/gpt-6.1-sol`) at `medium`, which delegates
   small work to luna and terra and the hardest to astra.

Skipped a question, or not on a terminal? Each step can be run on its own:

```sh
cowboy models import chatgpt --all       # the models your account offers
cowboy crew new openai --preset chatgpt  # the built-in crew
cowboy crew use openai                   # make it what new sessions start with
cowboy models use -g chatgpt/gpt-6.1-sol # the model for solo work (`cowboy crew solo`)
```

In a session, `/model` and `/models` switch models, `/reasoning high` (or ←/→ in the
`/models` picker) changes how hard the model thinks, and `/crew` shows the routing.
Plan usage is subject to your plan's limits: when one is reached Cowboy says so
rather than retrying. Sign-in, tokens, limits and what plan usage supports are covered
in [ChatGPT sign-in](../using/chatgpt.md). To use the ChatGPT models beside other
providers, see [Add a second provider](../how-to.md#add-a-second-provider).

## Interactive mode is conversational

On a terminal, `cowboy` (no task, or with a seed task) is a persistent
conversational REPL — like Claude Code. The agent answers a turn, then returns to
the prompt for your next message, keeping the **full conversation and the same
sandbox** alive. The `final` tool ends a *turn*, not the session.

- **Ctrl-C** interrupts the current turn (you keep going); pressing it twice at an
  empty prompt ends the session. **F1** lists every key and command.
- **Piped / non-TTY** runs (`cowboy "task" | …`) stay single-shot, for scripting.

The default network policy allows common dev registries (npm, PyPI, Go, crates,
RubyGems, Debian, GitHub) so `npm`/`pip`/`go`/`cargo`/`apt` installs work out of
the box, including non-interactively.

## Common commands

```
cowboy                       # interactive conversational TUI (multi-turn)
cowboy "fix the tests"       # seed the conversation (TTY) / one-shot (piped)
cowboy init [--git]          # write .cowboy/{security,agent}.yaml
cowboy doctor                # environment checks
cowboy run <cmd>             # run a command in the sandbox
cowboy shell                 # interactive shell in the sandbox
cowboy patch show|save|apply|check|revert
cowboy proc list|start|stop|restart|logs <name>
cowboy skill list|show <name>
cowboy sessions              # list live/registered sessions
cowboy logs                  # list past sessions
cowboy replay <id>           # replay a past session
cowboy down [--all]          # end this project's (or every) session
cowboy completions zsh       # shell completion script
cowboy providers             # list, add, remove, login, logout (endpoints + credentials)
cowboy models list           # also: use, add, import, refresh, setup, migrate
cowboy crew list             # also: use, solo, new, copy, validate, usage
```

Plural and singular both work (`cowboy skill` / `cowboy skills`), `cowboy exec` is
`cowboy run`, and `-y`/`--yes` pre-answers the confirmation on anything destructive.

See the [CLI reference](../reference/cli.md) for the full, auto-generated command
tree, and [Ranch Plans](../ranch/overview.md) for multi-workstream orchestration.
